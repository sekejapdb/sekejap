//! `EXPLAIN` of every `battle50k` case, against the 2,000-row fixture.
//!
//! Three things are asserted, and they are the three `docs/QL_CONTRACT.md` §6
//! promises:
//!
//!   * the DRIVER is the one the case's shape names -- the spatial cover for
//!     a radius, the text merge for a term, the nearest walk for a KNN order;
//!   * every filter says HOW it is answered, and the ones the contract says
//!     are answered index-side say so;
//!   * the counters are the run's own, and the cases whose whole answer comes
//!     from postings report `primary_reads=0` -- which is the statement "a
//!     row is read only for projection or for a predicate the plan names as
//!     row-bound", measured rather than asserted in prose.
//!
//! The statements are the `e4-sql` arm's, written against the same schema, so
//! what is explained here is what the battery runs.

#[path = "sqlslice/fixture.rs"]
mod fixture;

use e4_prototype::sql::{Param, SqlResult};
use tempfile::TempDir;

fn open() -> (TempDir, fixture::Fixture) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let f = fixture::build(&path);
    (dir, f)
}

fn explain(f: &mut fixture::Fixture, sql: &str, params: &[Param]) -> String {
    match f
        .db
        .sql(&format!("EXPLAIN {sql}"), params)
        .unwrap_or_else(|e| panic!("EXPLAIN {sql}: {e}"))
    {
        SqlResult::Explain(text) => text,
        other => panic!("expected an explanation, got {other:?}"),
    }
}

fn counter(text: &str, name: &str) -> u64 {
    let work = text
        .lines()
        .find(|line| line.starts_with("work: "))
        .unwrap_or_else(|| panic!("no work line in:\n{text}"));
    for field in work.split_whitespace() {
        if let Some(value) = field.strip_prefix(&format!("{name}=")) {
            return value.parse().unwrap();
        }
    }
    panic!("no `{name}` counter in:\n{text}");
}

fn rows_of(text: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix("rows: "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no rows line in:\n{text}"))
}

/// The twenty-two statements, in battery order, as the `e4-sql` arm writes
/// them: `_id` in the select list, the predicate spelled the Tier-1 way.
fn battery(f: &fixture::Fixture) -> Vec<(&'static str, String, Vec<Param>, Option<usize>)> {
    let centre = fixture::centre();
    let (lon, lat) = (centre.longitude(), centre.latitude());
    let (w, e) = (lon - 0.2, lon + 0.2);
    let (s, n) = (lat - 0.15, lat + 0.15);
    let polygon = serde_json::json!({
        "type": "Polygon",
        "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]]
    })
    .to_string();
    let vector = fixture::query_vector();
    let _ = f;
    vec![
        (
            "pt_radius",
            format!(
                "SELECT _id FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 5000, true)"
            ),
            vec![],
            None,
        ),
        (
            "pt_bbox",
            format!(
                "SELECT _id FROM place WHERE ST_Within(loc::geometry, ST_MakeEnvelope({w:?},{s:?},{e:?},{n:?},4326))"
            ),
            vec![],
            None,
        ),
        (
            "plot_within_box",
            format!(
                "SELECT _id FROM place WHERE ST_Within(plot::geometry, ST_MakeEnvelope({w:?},{s:?},{e:?},{n:?},4326))"
            ),
            vec![],
            None,
        ),
        (
            "plot_contains_pt",
            format!(
                "SELECT _id FROM place WHERE ST_Contains(plot::geometry, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326))"
            ),
            vec![],
            None,
        ),
        (
            "plot_intersects",
            "SELECT _id FROM place WHERE ST_Intersects(plot, ST_SetSRID(ST_GeomFromGeoJSON($1),4326)::geography)".to_owned(),
            vec![Param::Text(polygon.clone())],
            None,
        ),
        (
            "plot_dwithin_1km",
            format!(
                "SELECT _id FROM place WHERE ST_DWithin(plot, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 1000, true)"
            ),
            vec![],
            None,
        ),
        (
            "plot_vs_poly_within",
            "SELECT _id FROM place WHERE ST_Within(plot::geometry, ST_SetSRID(ST_GeomFromGeoJSON($1),4326))".to_owned(),
            vec![Param::Text(polygon)],
            None,
        ),
        (
            "text_one",
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1)".to_owned(),
            vec![Param::Text("kebun".into())],
            None,
        ),
        (
            "text_two",
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1)".to_owned(),
            vec![Param::Text("kebun & sawah".into())],
            None,
        ),
        (
            "text_and_kind",
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) AND kind = $2".to_owned(),
            vec![Param::Text("kebun".into()), Param::Text("park".into())],
            None,
        ),
        (
            "born_range",
            "SELECT _id FROM place WHERE born BETWEEN $1 AND $2".to_owned(),
            vec![Param::Int(19_500_101), Param::Int(19_510_101)],
            None,
        ),
        (
            "kind_eq",
            "SELECT _id FROM place WHERE kind = $1".to_owned(),
            vec![Param::Text("park".into())],
            None,
        ),
        (
            "radius_and_born",
            format!(
                "SELECT _id FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 20000, true) AND born BETWEEN 19500101 AND 19510101"
            ),
            vec![],
            None,
        ),
        (
            "knn_10",
            format!(
                "SELECT _id FROM place ORDER BY loc <-> ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography LIMIT 10"
            ),
            vec![],
            None,
        ),
        (
            "knn_10_kind",
            format!(
                "SELECT _id FROM place WHERE kind = $1 ORDER BY loc <-> ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography LIMIT 10"
            ),
            vec![Param::Text("park".into())],
            None,
        ),
        (
            "text_top10",
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) \
             ORDER BY ts_rank_cd(to_tsvector('simple', text), to_tsquery('simple', $1)) DESC LIMIT 10".to_owned(),
            vec![Param::Text("kebun".into())],
            None,
        ),
        (
            "vec_exact_10",
            "SELECT _id FROM place ORDER BY emb <=> $1::vector LIMIT 10".to_owned(),
            vec![Param::Vector(vector.clone())],
            None,
        ),
        (
            "vec_exact_radius",
            format!(
                "SELECT _id FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 20000, true) ORDER BY emb <=> $1::vector LIMIT 10"
            ),
            vec![Param::Vector(vector.clone())],
            None,
        ),
        (
            "hybrid_10",
            format!(
                "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $2) \
                 AND ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 20000, true) \
                 ORDER BY emb <=> $1::vector LIMIT 10"
            ),
            vec![
                Param::Vector(vector.clone()),
                Param::Text("kebun".into()),
            ],
            None,
        ),
        (
            "hybrid_blend_10",
            format!(
                "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $2) \
                 AND ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 20000, true) \
                 ORDER BY 0.5 * bm25(text, $2) + 0.5 * (1 - (emb <=> $1::vector)) DESC LIMIT 10"
            ),
            vec![
                Param::Vector(vector.clone()),
                Param::Text("kebun".into()),
            ],
            None,
        ),
        (
            "vec_ann_10@ef100",
            "SELECT _id FROM place ORDER BY emb <=> $1::vector LIMIT 10".to_owned(),
            vec![Param::Vector(vector.clone())],
            Some(100),
        ),
        (
            "vec_ann_10_kind@ef100",
            "SELECT _id FROM place WHERE kind = $1 ORDER BY emb <=> $2::vector LIMIT 10".to_owned(),
            vec![Param::Text("park".into()), Param::Vector(vector)],
            Some(100),
        ),
    ]
}

#[test]
fn every_battery_case_explains_its_driver_its_filters_and_its_counters() {
    let (_dir, mut f) = open();
    // The driver each case's shape names, and the EXACT number of primary
    // rows its answer reads. Zero is the contract's promise (§6: a row is
    // read only for projection or for a predicate the plan names as
    // row-bound) and it holds for fourteen of the twenty-two; the eight that
    // do read rows each have a named reason, below.
    let expected: Vec<(&str, &str, u64, &str)> = vec![
        ("pt_radius", "Spatial", 0, "the cover walk proves the radius from the posting's own coordinates"),
        ("pt_bbox", "Spatial", 0, "the cover walk proves the rectangle the same way"),
        ("plot_within_box", "Geometry", 500, "a geometry posting's box ADMITS; the predicate is refined from the row"),
        ("plot_contains_pt", "Geometry", 1, "one candidate admitted, one row refined"),
        ("plot_intersects", "Geometry", 500, "refined from the row, as above"),
        ("plot_dwithin_1km", "Geometry", 4, "four candidates admitted, four rows refined"),
        ("plot_vs_poly_within", "Geometry", 500, "refined from the row, as above"),
        ("text_one", "Text", 0, "a non-phrase text match is settled by the postings"),
        ("text_two", "Text", 0, "an all-terms merge is still postings only"),
        ("text_and_kind", "Scalar", 0, "the narrower scalar equality drives; the text filter is probed from its postings"),
        ("born_range", "Scalar", 0, "one posting range, no row"),
        ("kind_eq", "Scalar", 0, "one equality posting range, no row"),
        ("radius_and_born", "Spatial", 0, "the cover certifies the radius and the born range becomes a membership set"),
        ("knn_10", "Nearest", 0, "the ring walk yields the ten nearest in order"),
        ("knn_10_kind", "Nearest", 0, "the kind equality becomes a membership set beside the ring walk"),
        ("text_top10", "Text", 0, "a BM25 winner is proved live by its norm, so the page keeps no re-fetch"),
        ("vec_exact_10", "ExactVector", 10, "a vector page re-fetches its ten winners to prove them live"),
        ("vec_exact_radius", "Spatial", 10, "same ten winners, under the radius driver"),
        ("hybrid_10", "Text", 0, "the text merge drives and the radius is a membership set"),
        ("hybrid_blend_10", "Text", 0, "a Score page ranks from index leaves alone"),
        ("vec_ann_10@ef100", "QuantizedVector", 10, "the approximate shortlist is reranked from f32 sidecars; the ten winners are re-fetched"),
        ("vec_ann_10_kind@ef100", "QuantizedVector", 10, "the compact scan drives, the kind equality is a membership set, and the ten winners are re-fetched"),
    ];
    let cases = battery(&f);
    assert_eq!(cases.len(), expected.len());
    for ((name, sql, params, ef), (expected_name, driver, primary_reads, why)) in
        cases.iter().zip(&expected)
    {
        assert_eq!(name, expected_name);
        match ef {
            Some(ef) => {
                f.db.sql(&format!("SET LOCAL ef_search = {ef}"), &[]).unwrap();
            }
            None => {
                f.db.sql("SET LOCAL ef_search = DEFAULT", &[]).unwrap();
            }
        }
        let text = explain(&mut f, sql, params);
        assert!(
            text.starts_with(&format!("driver: {driver}")),
            "{name}: expected the {driver} driver\n{text}"
        );
        assert!(text.contains("\n  walks: "), "{name}:\n{text}");
        assert!(text.contains("\norder: "), "{name}:\n{text}");
        assert!(text.contains("\nrows: "), "{name}:\n{text}");
        assert!(rows_of(&text) > 0, "{name} matched nothing:\n{text}");
        assert_eq!(
            counter(&text, "primary_reads"),
            *primary_reads,
            "{name} ({why}):\n{text}"
        );
        // Every filter position says how it is answered, with no blanks.
        for line in text.lines().filter(|line| line.trim_start().starts_with('[')) {
            assert!(line.contains(" -> "), "{name}: `{line}` says nothing");
        }
    }
}

#[test]
fn a_filter_says_how_it_is_answered() {
    let (_dir, mut f) = open();
    // A radius under the cover walk: the driving postings certify it, so the
    // row has nothing to add.
    let centre = fixture::centre();
    let text = explain(
        &mut f,
        &format!(
            "SELECT _id FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({:?},{:?}),4326)::geography, 5000, true)",
            centre.longitude(),
            centre.latitude()
        ),
        &[],
    );
    assert!(
        text.contains("[0] point place_loc (loc) radius 5000.0 m"),
        "{text}"
    );
    assert!(
        text.contains("-> index posting: the driving walk certifies it"),
        "{text}"
    );

    // A geometry predicate is refined from the row: the posting box is a
    // candidate test, not a proof, and EXPLAIN says so rather than claiming
    // the index answered it.
    let text = explain(
        &mut f,
        &format!(
            "SELECT _id FROM place WHERE ST_DWithin(plot, ST_SetSRID(ST_MakePoint({:?},{:?}),4326)::geography, 1000, true)",
            centre.longitude(),
            centre.latitude()
        ),
        &[],
    );
    assert!(text.contains("geometry place_plot (plot)"), "{text}");
    assert!(text.contains("-> row"), "{text}");
    assert!(text.contains("box admits then the row refines"), "{text}");

    // A scalar range beside a driving text merge becomes a membership set
    // built once from postings.
    let text = explain(
        &mut f,
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', 'kebun') AND born BETWEEN 19500101 AND 19510101",
        &[],
    );
    assert!(text.starts_with("driver: Text"), "{text}");
    assert!(
        text.contains("membership set built from postings"),
        "{text}"
    );
}

#[test]
fn a_scan_by_definition_is_labelled_as_one() {
    let (_dir, mut f) = open();
    let text = explain(
        &mut f,
        "SELECT _id FROM place ORDER BY emb <=> $1::vector LIMIT 10",
        &[Param::Vector(fixture::query_vector())],
    );
    assert!(text.starts_with("driver: ExactVector"), "{text}");
    assert!(text.contains("SCAN by definition"), "{text}");
    assert!(text.contains("QL_CONTRACT §6"), "{text}");

    // A filtered walk is not a scan and is not labelled one.
    let text = explain(
        &mut f,
        "SELECT _id FROM place WHERE kind = 'park'",
        &[],
    );
    assert!(!text.contains("SCAN by definition"), "{text}");
}

#[test]
fn an_approximate_order_reports_its_shortlist() {
    let (_dir, mut f) = open();
    f.db.sql("SET LOCAL ef_search = 40", &[]).unwrap();
    let text = explain(
        &mut f,
        "SELECT _id FROM place ORDER BY emb <=> $1::vector LIMIT 10",
        &[Param::Vector(fixture::query_vector())],
    );
    assert!(text.starts_with("driver: QuantizedVector"), "{text}");
    assert!(text.contains("ef=40"), "{text}");
    assert!(text.contains("approximation: "), "{text}");
    assert!(text.contains("examined="), "{text}");
    assert!(text.contains("reranked="), "{text}");
    f.db.sql("COMMIT", &[]).unwrap();
}

#[test]
fn a_score_order_names_every_leaf() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let text = explain(
        &mut f,
        &format!(
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $2) \
             AND ST_DWithin(loc, ST_SetSRID(ST_MakePoint({:?},{:?}),4326)::geography, 20000, true) \
             ORDER BY 0.5 * bm25(text, $2) + 0.5 * (1 - (emb <=> $1::vector)) DESC LIMIT 10",
            centre.longitude(),
            centre.latitude()
        ),
        &[
            Param::Vector(fixture::query_vector()),
            Param::Text("kebun".into()),
        ],
    );
    assert!(text.contains("order: score"), "{text}");
    assert!(text.contains("score leaves:"), "{text}");
    for leaf in [
        "literal 0.5",
        "bm25 place_text",
        "literal 1",
        "vector_similarity place_emb_exact (Cosine)",
    ] {
        assert!(text.contains(leaf), "missing `{leaf}` in:\n{text}");
    }
}

#[test]
fn the_counters_are_the_ones_the_direct_api_charges() {
    use e4_prototype::collections::{
        CandidateDriver, PointFilter, Projection, QueryBudget, QueryFilter, QueryOrder,
        QueryRequest,
    };
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let mut prepared = f
        .db
        .prepare_query(QueryRequest {
            collection: f.place,
            filters: &[QueryFilter::Point {
                index: f.index.loc,
                predicate: PointFilter::Radius {
                    center: centre,
                    radius_metres: 5_000.0,
                },
            }],
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut candidates = 0u64;
    let mut spatial = 0u64;
    let mut rows = 0u64;
    loop {
        let page = prepared
            .next_page(8192, QueryBudget::unlimited(), || false)
            .unwrap();
        candidates += page.work.candidates;
        spatial += page.work.spatial_postings;
        rows += page.rows.len() as u64;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    let text = explain(
        &mut f,
        &format!(
            "SELECT _id FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({:?},{:?}),4326)::geography, 5000, true)",
            centre.longitude(),
            centre.latitude()
        ),
        &[],
    );
    assert_eq!(rows_of(&text), rows);
    assert_eq!(counter(&text, "candidates"), candidates);
    assert_eq!(counter(&text, "spatial_postings"), spatial);
}

/// Five explanations, printed in full.
///
/// One per driver family the battery exercises, so the report can show what
/// `EXPLAIN` actually says rather than describe it. Run with `--nocapture` to
/// see them; the assertions below are what makes this a test rather than a
/// print.
#[test]
fn five_explanations_in_full() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let (lon, lat) = (centre.longitude(), centre.latitude());
    let vector = fixture::query_vector();
    let samples: Vec<(&str, String, Vec<Param>)> = vec![
        (
            "pt_radius",
            format!(
                "SELECT _id FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 5000, true)"
            ),
            vec![],
        ),
        (
            "text_and_kind",
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) AND kind = $2".to_owned(),
            vec![Param::Text("kebun".into()), Param::Text("park".into())],
        ),
        (
            "plot_dwithin_1km",
            format!(
                "SELECT _id FROM place WHERE ST_DWithin(plot, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 1000, true)"
            ),
            vec![],
        ),
        (
            "knn_10",
            format!(
                "SELECT _id FROM place ORDER BY loc <-> ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography LIMIT 10"
            ),
            vec![],
        ),
        (
            "hybrid_blend_10",
            format!(
                "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $2) \
                 AND ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 20000, true) \
                 ORDER BY 0.5 * bm25(text, $2) + 0.5 * (1 - (emb <=> $1::vector)) DESC LIMIT 10"
            ),
            vec![Param::Vector(vector), Param::Text("kebun".into())],
        ),
    ];
    for (name, sql, params) in &samples {
        let text = explain(&mut f, sql, params);
        println!("===== {name}\n{sql}\n-----\n{text}");
        assert!(text.starts_with("driver: "), "{name}");
        assert!(text.contains("\nwork: "), "{name}");
    }
}

/// `EXPLAIN DROP TABLE` is the one EXPLAIN here that does not run its
/// statement: running a destructive DDL to print its plan would be the drop.
/// What it prints instead is the phase list, the bound each step honours and
/// the indexes the first phase has to remove.
#[test]
fn explain_drop_table_prints_the_phases_and_runs_nothing() {
    let (_dir, mut f) = open();
    let text = explain(&mut f, "DROP TABLE place CASCADE", &[]);
    println!("{text}");
    for expected in [
        "DROP TABLE place CASCADE",
        "does not run its statement",
        "begin_drop_collection publishes DROPPING",
        "  indexes -- 9 index(es)",
        "place_emb_ann",
        "  vector sidecars -- prefix 0x60",
        "  rows -- prefix 0x40",
        "cascade_graph_delete",
        "  external-key mappings -- prefix 0x20",
        "  descriptor --",
        "range probe proves every keyspace above is empty",
        "budget in 1..=256",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }
    assert_eq!(
        f.db.scan(f.place, None).unwrap().count(),
        fixture::ROWS,
        "EXPLAIN ran nothing"
    );
    // RESTRICT is the default, and the plan says which one it printed.
    let restrict = explain(&mut f, "DROP TABLE place", &[]);
    assert!(restrict.contains("RESTRICT (the default)"), "{restrict}");
}

// ── the aggregate battery (QL_CONTRACT §4.7) ──────────────────────────────
//
// EXPLAIN is where the shape claim is checked: streaming or hashed, where the
// group key comes from, where every accumulator reads its input, and how many
// groups the walk opened.

fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in:\n{text}"))
}

#[test]
fn agg_count_all_is_streaming_over_the_key_order_driver_and_reads_no_row() {
    let (_dir, mut f) = open();
    let text = explain(&mut f, "SELECT count(*) FROM place", &[]);
    println!("{text}");
    assert_eq!(line(&text, "shape: "), "shape: streaming");
    assert!(text.contains("driver: Keys"), "{text}");
    assert_eq!(line(&text, "group: "), "group: none -- one group over every candidate");
    assert!(
        text.contains("count(*) -> nothing is read"),
        "count(*) reads no column:\n{text}"
    );
    assert_eq!(counter(&text, "primary_reads"), 0);
    assert_eq!(counter(&text, "groups"), 1);
    assert_eq!(rows_of(&text), 1, "one group");
}

#[test]
fn agg_count_kind_streams_off_the_driving_posting() {
    let (_dir, mut f) = open();
    let text = explain(&mut f, "SELECT kind, count(*) AS n FROM place GROUP BY kind", &[]);
    println!("{text}");
    assert_eq!(line(&text, "shape: "), "shape: streaming");
    assert!(
        line(&text, "group: ").contains("the driving walk carries the value"),
        "{text}"
    );
    assert_eq!(counter(&text, "primary_reads"), 0, "no row is read:\n{text}");
    assert_eq!(counter(&text, "groups"), 1, "one accumulator set is alive");
    assert_eq!(rows_of(&text), fixture::KINDS.len() as u64);
}

#[test]
fn agg_sum_born_by_kind_names_the_row_its_accumulators_read() {
    let (_dir, mut f) = open();
    let text = explain(
        &mut f,
        "SELECT kind, count(*) AS n, sum(born) AS s, min(born) AS lo, max(born) AS hi, \
         avg(born) AS mean FROM place GROUP BY kind HAVING count(*) > 100",
        &[],
    );
    println!("{text}");
    assert_eq!(line(&text, "shape: "), "shape: streaming");
    // `born` is not the driving index, so every accumulator over it reads the
    // row -- and EXPLAIN says so rather than leaving it to be guessed.
    assert!(text.contains("sum(born) -> row (charged as primary_reads)"), "{text}");
    assert!(text.contains("avg(born) -> row (charged as primary_reads)"), "{text}");
    assert!(text.contains("having:"), "{text}");
    assert!(
        counter(&text, "primary_reads") >= fixture::ROWS as u64,
        "a row-side accumulator reads one row per candidate:\n{text}"
    );
}

#[test]
fn agg_distinct_kind_is_a_group_with_no_accumulators() {
    let (_dir, mut f) = open();
    let text = explain(&mut f, "SELECT DISTINCT kind FROM place", &[]);
    println!("{text}");
    assert_eq!(line(&text, "shape: "), "shape: streaming");
    assert!(text.contains("none -- a group with no accumulators is DISTINCT"), "{text}");
    assert_eq!(counter(&text, "primary_reads"), 0, "{text}");
    assert_eq!(rows_of(&text), fixture::KINDS.len() as u64);
}

#[test]
fn agg_count_radius_by_kind_hashes_because_the_radius_drives() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let (lon, lat) = (centre.longitude(), centre.latitude());
    let text = explain(
        &mut f,
        &format!(
            "SELECT kind, count(*) AS n FROM place \
             WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 20000, true) \
             GROUP BY kind"
        ),
        &[],
    );
    println!("{text}");
    assert_eq!(line(&text, "shape: "), "shape: hashed");
    assert!(text.contains("driver: Spatial"), "{text}");
    assert!(
        line(&text, "group: ").contains("row (charged as primary_reads)"),
        "the group key is not the driving walk's value:\n{text}"
    );
    assert!(counter(&text, "groups") > 1, "{text}");
    assert!(counter(&text, "groups") <= fixture::KINDS.len() as u64, "{text}");
}

#[test]
fn agg_born_decade_computes_its_expression_key_index_side() {
    let (_dir, mut f) = open();
    let text = explain(
        &mut f,
        "SELECT born / 10000 AS decade, count(*) AS n FROM place GROUP BY born / 10000",
        &[],
    );
    println!("{text}");
    assert_eq!(line(&text, "shape: "), "shape: streaming");
    assert!(
        line(&text, "group: ").contains("born / 10000"),
        "{text}"
    );
    assert!(
        line(&text, "group: ").contains("the driving walk carries the value"),
        "the expression is computed from the posting, not from the row:\n{text}"
    );
    assert_eq!(counter(&text, "primary_reads"), 0, "{text}");}


// ── GRAPH_TABLE: the per-hop predicates, printed ──────────────────────────

/// A weighted edge type beside the fixture's own, so an inline element WHERE
/// has a property bag to read. Written here rather than in the fixture
/// because every other suite over that fixture reads the same edges.
fn weighted_graph(f: &mut fixture::Fixture) -> e4_prototype::collections::EdgeTypeId {
    let weighted = f.db.create_edge_type("weighted").unwrap();
    f.db.commit().unwrap();
    let ids: Vec<e4_prototype::collections::EntityId> = f
        .keys
        .iter()
        .map(|key| f.db.get(f.place, key).unwrap().unwrap().id)
        .collect();
    for (i, pair) in ids.windows(2).enumerate() {
        f.db.put_edge(
            f.context,
            pair[0],
            weighted,
            pair[1],
            &serde_json::json!({"weight": (i % 10) as f64 / 10.0}),
        )
        .unwrap();
        if (i + 1) % 256 == 0 {
            f.db.commit().unwrap();
        }
    }
    f.db.commit().unwrap();
    weighted
}

/// `EXPLAIN` of a pattern says which edges are never followed and which
/// nodes are never expanded, and names the membership set the node half is
/// answered from -- `docs/QL_CONTRACT.md` §6's "EXPLAIN prints which".
#[test]
fn explain_prints_the_edge_predicates_and_the_node_membership_sets() {
    let (_dir, mut f) = open();
    let _ = weighted_graph(&mut f);
    let text = explain(
        &mut f,
        "SELECT k, w FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[r:weighted WHERE r.weight > 0.2]->{1,4}\
            (b:place WHERE b.born BETWEEN 19500101 AND 19600101) \
            COLUMNS (b._key AS k, r.weight AS w)) \
         ORDER BY w DESC LIMIT 5",
        &[Param::Text("k00003".into())],
    );
    println!("===== graph_per_hop\n{text}");
    assert!(text.starts_with("driver: "));
    assert!(text.contains("Graph"), "{text}");
    // The edge half, spelled the way the plan holds it.
    assert!(text.contains("edge weight > 0.2"), "{text}");
    // The node half, and the set it is answered from.
    assert!(text.contains("node place_born.born range"), "{text}");
    assert!(
        text.contains("membership set") || text.contains("membership bitmap"),
        "{text}"
    );
    // The traversal IS the candidate stream here, so its own position is
    // certified by the walk and no filter position reads a row.
    assert!(
        text.contains("the driving walk certifies it"),
        "{text}"
    );
    assert_eq!(counter(&text, "row_decodes"), 0, "{text}");
    // The ranking is the reaching edge's own property.
    assert!(
        text.contains("order: edge -- reaching edge property `weight` Descending"),
        "{text}"
    );
    // The projection names the edge spelling, not a declared field.
    assert!(text.contains("@edge.weight"), "{text}");
    assert!(counter(&text, "graph_edges") > 0, "{text}");
    assert!(rows_of(&text) > 0, "{text}");
}

/// The six boolean cases of the battery (`docs/QL_CONTRACT.md` §3), each
/// explained: the driver, the set the filter was walked into, and the rows
/// the answer read.
///
/// The driver is the interesting half. A disjunction never takes the driver
/// away from a conjunct that can narrow the candidates further, so
/// `bool_born_or_kind` under a second predicate would still be a scalar
/// range; on its own it is the union set itself, walked in id order.
/// A boolean filter is a pure in-memory bit test, so it is answered BEFORE
/// the row is read -- not by turning the batched row pass off.
///
/// `filters_are_row_pure` used to say false for `Boolean`, which switched off
/// both `batches_row_reads` and the borrowed-row path: the same geometry
/// query with `kind IN ('a','b')` beside it went from one ordered batch
/// cursor to a point-get and a row copy per geometry candidate, a cliff
/// against the identical query with `kind = 'a'`. The bit test now stands in
/// FRONT of the read, which is visible as rows that are never decoded.
#[test]
fn a_boolean_filter_is_answered_before_the_row_is_read() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let (lon, lat) = (centre.longitude(), centre.latitude());
    let geometry = format!(
        "ST_DWithin(plot, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 40000, true)"
    );
    let bare = explain(
        &mut f,
        &format!("SELECT _id FROM place WHERE {geometry}"),
        &[],
    );
    let boolean = explain(
        &mut f,
        &format!("SELECT _id FROM place WHERE {geometry} AND kind IN ('depot','farm')"),
        &[],
    );
    // The geometry walk keeps driving: a union never drives by preference
    // (`docs/QL_CONTRACT.md` §3), and it no longer costs the walk its shape
    // either.
    let driver = |text: &str| text.lines().next().unwrap().to_owned();
    assert_eq!(driver(&boolean), driver(&bare), "{boolean}");
    // And the candidates the bit test rejects cost no row: fewer decodes than
    // the same geometry walk with nothing beside it.
    let decodes = |text: &str| counter(text, "row_decodes");
    assert!(
        decodes(&boolean) < decodes(&bare),
        "the bit test stands in front of the row read: boolean={} bare={}\n{boolean}",
        decodes(&boolean),
        decodes(&bare)
    );
    assert!(
        counter(&boolean, "primary_reads") < counter(&bare, "primary_reads"),
        "and in front of the primary read too:\n{boolean}"
    );
    // The answer is the equality's answer, unioned: the plan changed, the
    // rows did not. (The equality form takes the SCALAR driver, because an
    // equality drives and a union by design does not.)
    let rows_in = |sql: &str, f: &mut fixture::Fixture| rows_of(&explain(f, sql, &[]));
    let depot = rows_in(
        &format!("SELECT _id FROM place WHERE {geometry} AND kind = 'depot'"),
        &mut f,
    );
    let farm = rows_in(
        &format!("SELECT _id FROM place WHERE {geometry} AND kind = 'farm'"),
        &mut f,
    );
    assert_eq!(rows_of(&boolean), depot + farm, "{boolean}");
}

#[test]
fn the_boolean_battery_explains_its_sets_and_its_counters() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let (lon, lat) = (centre.longitude(), centre.latitude());
    let cases: Vec<(&str, String, Vec<Param>, &str, &str, u64)> = vec![
        (
            "bool_kind_in3",
            "SELECT _id FROM place WHERE kind IN ($1, $2, $3)".to_owned(),
            vec![
                Param::Text("depot".into()),
                Param::Text("farm".into()),
                Param::Text("home".into()),
            ],
            "Membership",
            "union: union(",
            0,
        ),
        (
            "bool_born_or_kind",
            "SELECT _id FROM place WHERE (born BETWEEN $1 AND $2) OR kind = $3".to_owned(),
            vec![
                Param::Int(19_500_101),
                Param::Int(19_520_101),
                Param::Text("mill".into()),
            ],
            "Membership",
            "union: union(",
            0,
        ),
        (
            "bool_not_kind",
            "SELECT _id FROM place WHERE kind <> $1".to_owned(),
            vec![Param::Text("depot".into())],
            "Membership",
            "complement: union(",
            0,
        ),
        (
            "bool_radius_or_radius",
            format!(
                "SELECT _id FROM place \
                 WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 5000, true) \
                 OR ST_DWithin(loc, ST_SetSRID(ST_MakePoint({:?},{lat:?}),4326)::geography, 5000, true)",
                lon + 0.3
            ),
            vec![],
            "Membership",
            "union: union(",
            0,
        ),
        (
            "bool_not_null_born",
            "SELECT _id FROM place WHERE born IS NOT NULL".to_owned(),
            vec![],
            "Membership",
            "complement: place_born.born range",
            0,
        ),
    ];
    for (name, sql, params, driver, detail, primary_reads) in cases {
        let text = explain(&mut f, &sql, &params);
        assert!(
            text.starts_with(&format!("driver: {driver}")),
            "{name}: expected the {driver} driver\n{text}"
        );
        assert!(rows_of(&text) > 0, "{name} matched nothing:\n{text}");
        let line = text
            .lines()
            .find(|line| line.trim_start().starts_with("[0] boolean"))
            .unwrap_or_else(|| panic!("{name}: no boolean filter line in\n{text}"));
        assert!(
            line.contains(detail),
            "{name}: expected `{detail}` in `{line}`"
        );
        assert!(
            line.contains(" ids") || line.contains("bitmap,"),
            "{name}: the set's size is printed: `{line}`"
        );
        assert!(
            line.contains("the driving walk certifies it"),
            "{name}: the union IS the driver, so it certifies itself: `{line}`"
        );
        assert_eq!(
            counter(&text, "primary_reads"),
            primary_reads,
            "{name}: a boolean answer reads no row for its predicate\n{text}"
        );
    }
}

/// `bool_exists_related`'s shape, on the fixture's own base-graph edges: a
/// semi-join set, applied as a filter, with the complement beside it.
#[test]
fn a_semi_join_explains_the_set_it_built() {
    let (_dir, mut f) = open();
    let linked = f.db.create_edge_type("linked").unwrap();
    f.db.commit().unwrap();
    for at in (0..fixture::ROWS).step_by(4) {
        let from = f.db.get(f.place, &f.rows[at].key).unwrap().unwrap().id;
        let to = f
            .db
            .get(f.place, &f.rows[(at + 1) % fixture::ROWS].key)
            .unwrap()
            .unwrap()
            .id;
        f.db.put_edge(
            e4_prototype::collections::GraphContextId::BASE,
            from,
            linked,
            to,
            &serde_json::json!({}),
        )
        .unwrap();
    }
    f.db.commit().unwrap();
    let text = explain(
        &mut f,
        "SELECT _id FROM place WHERE EXISTS (SELECT 1 FROM linked WHERE source = _key)",
        &[],
    );
    assert!(text.starts_with("driver: Membership"), "{text}");
    assert!(text.contains("set: semi-join set, 500 ids"), "{text}");
    assert_eq!(rows_of(&text), 500, "{text}");
    // The ids came from the CALLER (the subquery), not from a posting, so
    // this page keeps the existence probe its winners owe: one primary read
    // per returned row. `SetExpr::proves_live` is where that is decided.
    assert_eq!(counter(&text, "primary_reads"), 500, "{text}");

    let negated = explain(
        &mut f,
        "SELECT _id FROM place WHERE NOT EXISTS (SELECT 1 FROM linked WHERE source = _key)",
        &[],
    );
    assert!(
        negated.contains("complement: complement(semi-join set, 500 ids)"),
        "{negated}"
    );
    assert_eq!(rows_of(&negated), (fixture::ROWS - 500) as u64, "{negated}");
}
