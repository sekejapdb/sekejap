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
