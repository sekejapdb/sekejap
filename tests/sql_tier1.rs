//! One test per Tier-1 statement family of `docs/QL_CONTRACT.md`, written as
//! LITERAL SQL against a 2,000-row fixture, each asserting that the rows the
//! statement returns are the rows the direct API returns for the same
//! question.
//!
//! The direct API is the oracle on purpose: the SQL layer's whole claim is
//! that it compiles to the calls the crate already has and adds no second
//! engine, so the only interesting failure is a statement that answers a
//! DIFFERENT question than the `QueryRequest` a caller would have written.

#[path = "sqlslice/fixture.rs"]
mod fixture;

use e4_prototype::{
    collections::{
        CandidateDriver, Database, EntityId, Geom, GeometryFilter, PointFilter, Projection,
        QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue, ScoreExpr,
        SortDirection, TextMatch, VectorMetric,
    },
    spatial_math::Bounds,
    sql::{Param, SqlResult, SqlValue},
};
use std::ops::Bound;
use tempfile::TempDir;

const PAGE: usize = 8192;

fn open() -> (TempDir, fixture::Fixture) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let f = fixture::build(&path);
    (dir, f)
}

/// Every row the direct API returns for one request, in result order.
fn direct(
    db: &Database,
    collection: e4_prototype::collections::CollectionId,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    limit: Option<usize>,
) -> Vec<EntityId> {
    // A key filter is meaningful only under the driver that certifies it, so
    // the oracle names the same driver the compiler picks.
    let driver = if filters
        .iter()
        .any(|filter| matches!(filter, QueryFilter::Key { .. }))
    {
        CandidateDriver::Keys
    } else {
        CandidateDriver::Auto
    };
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection,
            filters,
            order,
            projection: Projection::Ids,
            total_limit: limit,
            driver,
        })
        .unwrap();
    let mut out = Vec::new();
    loop {
        let page = prepared
            .next_page(PAGE, QueryBudget::unlimited(), || false)
            .unwrap();
        out.extend(page.rows.iter().map(|row| row.id));
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    out
}

/// Every row one statement returns, in result order.
fn sql_ids(db: &mut Database, text: &str, params: &[Param]) -> Vec<EntityId> {
    match db.sql(text, params).unwrap() {
        SqlResult::Rows { rows, .. } => rows.iter().map(|row| row.id).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn sql_rows(db: &mut Database, text: &str, params: &[Param]) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match db.sql(text, params).unwrap() {
        SqlResult::Rows { columns, rows } => {
            (columns, rows.into_iter().map(|row| row.values).collect())
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

fn affected(db: &mut Database, text: &str, params: &[Param]) -> u64 {
    match db.sql(text, params).unwrap() {
        SqlResult::Affected(n) => n,
        other => panic!("expected an affected count, got {other:?}"),
    }
}

// ── SELECT: projection ────────────────────────────────────────────────────

#[test]
fn select_star_names_every_declared_column() {
    let (_dir, mut f) = open();
    let (columns, rows) = sql_rows(
        &mut f.db,
        "SELECT * FROM place WHERE kind = $1 LIMIT 3",
        &[Param::Text("depot".into())],
    );
    assert_eq!(
        columns,
        vec![
            "key", "name", "descr", "text", "born", "kind", "loc", "plot", "emb", "score", "flag",
            "tag"
        ]
    );
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row.len(), columns.len());
        assert_eq!(row[5], SqlValue::Text("depot".into()));
    }
}

#[test]
fn selecting_the_row_identity_reads_no_field() {
    let (_dir, mut f) = open();
    let (columns, rows) = sql_rows(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = $1",
        &[Param::Text("farm".into())],
    );
    assert_eq!(columns, vec!["_id"]);
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|row| matches!(row[0], SqlValue::Id(_))));
}

#[test]
fn selecting_the_external_key_returns_the_key_the_row_was_put_under() {
    let (_dir, mut f) = open();
    let (columns, rows) = sql_rows(
        &mut f.db,
        "SELECT _key FROM place WHERE born BETWEEN $1 AND $2 LIMIT 5",
        &[Param::Int(19_500_101), Param::Int(19_500_301)],
    );
    assert_eq!(columns, vec!["_key"]);
    assert!(!rows.is_empty());
    for row in &rows {
        let SqlValue::Text(key) = &row[0] else {
            panic!("the external key is text");
        };
        assert!(f.keys.contains(key), "`{key}` is not a fixture key");
    }
}

// ── WHERE: scalar predicates ──────────────────────────────────────────────

#[test]
fn scalar_equality_matches_the_direct_request() {
    let (_dir, mut f) = open();
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text("mill")),
        }],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = $1",
        &[Param::Text("mill".into())],
    );
    assert_eq!(got, expected);
    assert_eq!(expected.len(), fixture::ROWS / fixture::KINDS.len());
}

#[test]
fn scalar_range_and_between_match_the_direct_request() {
    let (_dir, mut f) = open();
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Scalar {
            index: f.index.born,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(19_500_101)),
                upper: Bound::Included(ScalarValue::I64(19_501_101)),
            },
        }],
        QueryOrder::Driver,
        None,
    );
    let between = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE born BETWEEN $1 AND $2",
        &[Param::Int(19_500_101), Param::Int(19_501_101)],
    );
    assert_eq!(between, expected);

    let open_ended = direct(
        &f.db,
        f.place,
        &[QueryFilter::Scalar {
            index: f.index.born,
            predicate: ScalarFilter::Range {
                lower: Bound::Excluded(ScalarValue::I64(19_560_101)),
                upper: Bound::Unbounded,
            },
        }],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE born > 19560101",
        &[],
    );
    assert_eq!(got, open_ended);
}

#[test]
fn is_null_and_is_missing_are_different_questions() {
    let (_dir, mut f) = open();
    let nulls = direct(
        &f.db,
        f.place,
        &[QueryFilter::Scalar {
            index: f.index.score,
            predicate: ScalarFilter::IsNull,
        }],
        QueryOrder::Driver,
        None,
    );
    assert_eq!(
        sql_ids(&mut f.db, "SELECT _id FROM place WHERE score IS NULL", &[]),
        nulls
    );
    assert!(!nulls.is_empty());

    let missing = direct(
        &f.db,
        f.place,
        &[QueryFilter::Scalar {
            index: f.index.score,
            predicate: ScalarFilter::IsMissing,
        }],
        QueryOrder::Driver,
        None,
    );
    assert_eq!(
        sql_ids(
            &mut f.db,
            "SELECT _id FROM place WHERE score IS MISSING",
            &[]
        ),
        missing
    );
    // `score` is written as an explicit JSON null, never omitted, so the two
    // predicates answer differently.
    assert!(missing.is_empty());
}

#[test]
fn a_conjunction_is_a_filter_list() {
    let (_dir, mut f) = open();
    let expected = direct(
        &f.db,
        f.place,
        &[
            QueryFilter::Scalar {
                index: f.index.kind,
                predicate: ScalarFilter::Eq(ScalarValue::Text("park")),
            },
            QueryFilter::Scalar {
                index: f.index.flag,
                predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
            },
        ],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = 'park' AND flag = TRUE",
        &[],
    );
    assert_eq!(got, expected);
}

#[test]
fn a_key_range_walks_the_mapping_keyspace() {
    let (_dir, mut f) = open();
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Key {
            lower: Bound::Included("k00010"),
            upper: Bound::Included("k00019"),
        }],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE _key BETWEEN $1 AND $2",
        &[Param::Text("k00010".into()), Param::Text("k00019".into())],
    );
    assert_eq!(got, expected);
    assert_eq!(got.len(), 10);
}

// ── WHERE: text ───────────────────────────────────────────────────────────

#[test]
fn a_tsquery_compiles_to_any_all_and_phrase() {
    let (_dir, mut f) = open();
    for (sql, query, matching) in [
        (
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', 'kebun | sawah')",
            "kebun sawah",
            TextMatch::Any,
        ),
        (
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', 'kebun & sawah')",
            "kebun sawah",
            TextMatch::All,
        ),
        (
            "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', '\"kebun sawah\"')",
            "kebun sawah",
            TextMatch::Phrase,
        ),
    ] {
        let expected = direct(
            &f.db,
            f.place,
            &[QueryFilter::Text {
                index: f.index.text,
                query,
                matching,
            }],
            QueryOrder::Driver,
            None,
        );
        assert_eq!(sql_ids(&mut f.db, sql, &[]), expected, "{sql}");
    }
}

#[test]
fn a_one_term_tsquery_is_any_of_one_term() {
    let (_dir, mut f) = open();
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Text {
            index: f.index.text,
            query: "kebun",
            matching: TextMatch::Any,
        }],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1)",
        &[Param::Text("kebun".into())],
    );
    assert_eq!(got, expected);
    assert!(!expected.is_empty());
}

// ── WHERE: spatial ────────────────────────────────────────────────────────

#[test]
fn st_dwithin_on_a_point_column_is_a_radius() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Point {
            index: f.index.loc,
            predicate: PointFilter::Radius {
                center: centre,
                radius_metres: 5_000.0,
            },
        }],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place \
         WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)",
        &[
            Param::Float(centre.longitude()),
            Param::Float(centre.latitude()),
            Param::Float(5_000.0),
        ],
    );
    assert_eq!(got, expected);
    assert!(!expected.is_empty());
}

#[test]
fn st_within_an_envelope_on_a_point_column_is_a_bbox() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let (w, e) = (centre.longitude() - 0.05, centre.longitude() + 0.05);
    let (s, n) = (centre.latitude() - 0.05, centre.latitude() + 0.05);
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Point {
            index: f.index.loc,
            predicate: PointFilter::Bbox(Bounds::new(w, e, s, n).unwrap()),
        }],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        &format!(
            "SELECT _id FROM place \
             WHERE ST_Within(loc::geometry, ST_MakeEnvelope({w:?}, {s:?}, {e:?}, {n:?}, 4326))"
        ),
        &[],
    );
    assert_eq!(got, expected);
    assert!(!expected.is_empty());
}

#[test]
fn the_four_geometry_predicates_match_the_direct_request() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let (w, e) = (centre.longitude() - 0.06, centre.longitude() + 0.06);
    let (s, n) = (centre.latitude() - 0.06, centre.latitude() + 0.06);
    let rectangle = Geom::Polygon(vec![vec![
        [w, s],
        [e, s],
        [e, n],
        [w, n],
        [w, s],
    ]]);
    let rectangle_json = fixture::geom_json(&rectangle).to_string();
    let point = Geom::Point(centre.longitude(), centre.latitude());

    for (predicate, sql) in [
        (
            GeometryFilter::Within(rectangle.clone()),
            format!(
                "SELECT _id FROM place WHERE ST_Within(plot::geometry, ST_MakeEnvelope({w:?}, {s:?}, {e:?}, {n:?}, 4326))"
            ),
        ),
        (
            GeometryFilter::Intersects(rectangle.clone()),
            "SELECT _id FROM place WHERE ST_Intersects(plot, ST_SetSRID(ST_GeomFromGeoJSON($1), 4326)::geography)".to_owned(),
        ),
        (
            GeometryFilter::Contains(point.clone()),
            format!(
                "SELECT _id FROM place WHERE ST_Contains(plot::geometry, ST_SetSRID(ST_MakePoint({:?}, {:?}), 4326))",
                centre.longitude(),
                centre.latitude()
            ),
        ),
        (
            GeometryFilter::DWithin {
                geometry: point.clone(),
                metres: 1_000.0,
            },
            format!(
                "SELECT _id FROM place WHERE ST_DWithin(plot, ST_SetSRID(ST_MakePoint({:?}, {:?}), 4326)::geography, 1000)",
                centre.longitude(),
                centre.latitude()
            ),
        ),
    ] {
        let expected = direct(
            &f.db,
            f.place,
            &[QueryFilter::Geometry {
                index: f.index.plot,
                predicate: predicate.clone(),
            }],
            QueryOrder::Driver,
            None,
        );
        let params = vec![Param::Text(rectangle_json.clone())];
        assert_eq!(sql_ids(&mut f.db, &sql, &params), expected, "{sql}");
    }
}

// ── ORDER BY ──────────────────────────────────────────────────────────────

#[test]
fn an_indexed_scalar_order_matches_the_direct_request() {
    let (_dir, mut f) = open();
    for (direction, sql) in [
        (
            SortDirection::Ascending,
            "SELECT _id FROM place WHERE kind = 'shop' ORDER BY born ASC LIMIT 20",
        ),
        (
            SortDirection::Descending,
            "SELECT _id FROM place WHERE kind = 'shop' ORDER BY born DESC LIMIT 20",
        ),
    ] {
        let expected = direct(
            &f.db,
            f.place,
            &[QueryFilter::Scalar {
                index: f.index.kind,
                predicate: ScalarFilter::Eq(ScalarValue::Text("shop")),
            }],
            QueryOrder::Scalar {
                index: f.index.born,
                direction,
            },
            Some(20),
        );
        assert_eq!(sql_ids(&mut f.db, sql, &[]), expected, "{sql}");
    }
}

#[test]
fn the_knn_operator_is_the_distance_order() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let expected = direct(
        &f.db,
        f.place,
        &[],
        QueryOrder::Distance {
            index: f.index.loc,
            center: centre,
            direction: SortDirection::Ascending,
        },
        Some(10),
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place ORDER BY loc <-> ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography LIMIT 10",
        &[
            Param::Float(centre.longitude()),
            Param::Float(centre.latitude()),
        ],
    );
    assert_eq!(got, expected);
    assert_eq!(got.len(), 10);
}

#[test]
fn the_cosine_operator_is_the_exact_vector_order() {
    let (_dir, mut f) = open();
    let vector = fixture::query_vector();
    let expected = direct(
        &f.db,
        f.place,
        &[],
        QueryOrder::ExactVector {
            index: f.index.emb_exact,
            query: &vector,
            metric: VectorMetric::Cosine,
        },
        Some(10),
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place ORDER BY emb <=> $1::vector LIMIT 10",
        &[Param::Vector(vector.clone())],
    );
    assert_eq!(got, expected);

    // The same statement with the vector written as pgvector's text literal.
    let literal = fixture::vector_literal(&vector);
    let got = sql_ids(
        &mut f.db,
        &format!("SELECT _id FROM place ORDER BY emb <=> '{literal}'::vector LIMIT 10"),
        &[],
    );
    assert_eq!(got, expected);
}

#[test]
fn set_local_ef_search_turns_the_vector_order_approximate() {
    let (_dir, mut f) = open();
    let vector = fixture::query_vector();
    assert!(matches!(
        f.db.sql("SET LOCAL ef_search = 64", &[]).unwrap(),
        SqlResult::Notice(_)
    ));
    let expected = direct(
        &f.db,
        f.place,
        &[],
        QueryOrder::ApproximateVector {
            index: f.index.emb_ann,
            query: &vector,
            metric: VectorMetric::Cosine,
            ef: 64,
        },
        Some(10),
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place ORDER BY emb <=> $1::vector LIMIT 10",
        &[Param::Vector(vector.clone())],
    );
    assert_eq!(got, expected);
    // COMMIT ends the transaction, and a LOCAL setting with it.
    f.db.sql("COMMIT", &[]).unwrap();
    let exact = direct(
        &f.db,
        f.place,
        &[],
        QueryOrder::ExactVector {
            index: f.index.emb_exact,
            query: &vector,
            metric: VectorMetric::Cosine,
        },
        Some(10),
    );
    let after = sql_ids(
        &mut f.db,
        "SELECT _id FROM place ORDER BY emb <=> $1::vector LIMIT 10",
        &[Param::Vector(vector)],
    );
    assert_eq!(after, exact);
}

#[test]
fn ts_rank_cd_and_bm25_are_the_same_order() {
    let (_dir, mut f) = open();
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Text {
            index: f.index.text,
            query: "kebun",
            matching: TextMatch::Any,
        }],
        QueryOrder::Bm25 {
            index: f.index.text,
            query: "kebun",
            matching: TextMatch::Any,
        },
        Some(10),
    );
    let by_rank = sql_ids(
        &mut f.db,
        "SELECT _id FROM place \
         WHERE to_tsvector('simple', text) @@ to_tsquery('simple', 'kebun') \
         ORDER BY ts_rank_cd(to_tsvector('simple', text), to_tsquery('simple', 'kebun')) DESC \
         LIMIT 10",
        &[],
    );
    assert_eq!(by_rank, expected);
    let by_bm25 = sql_ids(
        &mut f.db,
        "SELECT _id FROM place \
         WHERE to_tsvector('simple', text) @@ to_tsquery('simple', 'kebun') \
         ORDER BY bm25(text, 'kebun') DESC LIMIT 10",
        &[],
    );
    assert_eq!(by_bm25, expected);
}

#[test]
fn an_arithmetic_order_is_one_score_expression() {
    let (_dir, mut f) = open();
    let vector = fixture::query_vector();
    let centre = fixture::centre();
    // `1 - (emb <=> v)` is the cosine itself, which is `1 + VectorSimilarity`
    // under Cosine -- the same quantity battle50k's `hybrid_blend_10` names.
    let half = ScoreExpr::Lit(0.5);
    let one = ScoreExpr::Lit(1.0);
    let bm25 = ScoreExpr::Bm25 {
        index: f.index.text,
        query: "kebun",
        matching: TextMatch::Any,
    };
    let similarity = ScoreExpr::VectorSimilarity {
        index: f.index.emb_exact,
        query: &vector,
        metric: VectorMetric::Cosine,
    };
    let cosine = ScoreExpr::Add(&one, &similarity);
    let text_half = ScoreExpr::Mul(&half, &bm25);
    let vector_half = ScoreExpr::Mul(&half, &cosine);
    let blend = ScoreExpr::Add(&text_half, &vector_half);
    let expected = direct(
        &f.db,
        f.place,
        &[
            QueryFilter::Text {
                index: f.index.text,
                query: "kebun",
                matching: TextMatch::Any,
            },
            QueryFilter::Point {
                index: f.index.loc,
                predicate: PointFilter::Radius {
                    center: centre,
                    radius_metres: 20_000.0,
                },
            },
        ],
        QueryOrder::Score {
            expr: &blend,
            direction: SortDirection::Descending,
        },
        Some(10),
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place \
         WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) \
           AND ST_DWithin(loc, ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography, $4) \
         ORDER BY 0.5 * bm25(text, $1) + 0.5 * (1 - (emb <=> $5::vector)) DESC LIMIT 10",
        &[
            Param::Text("kebun".into()),
            Param::Float(centre.longitude()),
            Param::Float(centre.latitude()),
            Param::Float(20_000.0),
            Param::Vector(vector.clone()),
        ],
    );
    assert_eq!(got, expected);
    assert!(!got.is_empty());
}

#[test]
fn the_ranking_value_can_be_projected_under_an_alias() {
    let (_dir, mut f) = open();
    let (columns, rows) = sql_rows(
        &mut f.db,
        "SELECT _id, ts_rank_cd(to_tsvector('simple', text), to_tsquery('simple', 'kebun')) AS score \
         FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', 'kebun') \
         ORDER BY ts_rank_cd(to_tsvector('simple', text), to_tsquery('simple', 'kebun')) DESC LIMIT 5",
        &[],
    );
    assert_eq!(columns, vec!["_id", "score"]);
    let scores: Vec<f64> = rows
        .iter()
        .map(|row| match row[1] {
            SqlValue::Float(value) => value,
            ref other => panic!("a BM25 score is a float, got {other:?}"),
        })
        .collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
    assert!(scores[0] > 0.0);
}

// ── constants ─────────────────────────────────────────────────────────────

#[test]
fn a_scalar_subquery_is_a_constant() {
    let (_dir, mut f) = open();
    let born = f.rows[7].born;
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Scalar {
            index: f.index.born,
            predicate: ScalarFilter::Eq(ScalarValue::I64(born)),
        }],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE born = (SELECT born FROM place WHERE _key = $1)",
        &[Param::Text(f.rows[7].key.clone())],
    );
    assert_eq!(got, expected);
    assert!(!got.is_empty());
}

#[test]
fn a_parameter_takes_its_type_from_its_position() {
    let (_dir, mut f) = open();
    // The same `Param::Text` is read as a category, as a tsquery and as
    // GeoJSON, depending on where it is written.
    let category = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = $1",
        &[Param::Text("home".into())],
    );
    assert!(!category.is_empty());
    let text = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1)",
        &[Param::Text("kopi | danau".into())],
    );
    assert!(!text.is_empty());
    // An Int parameter in a Text column is refused rather than coerced.
    let refused = f
        .db
        .sql("SELECT _id FROM place WHERE kind = $1", &[Param::Int(3)])
        .unwrap_err();
    assert!(
        format!("{refused}").contains("declared"),
        "{refused}"
    );
}

// ── writes ────────────────────────────────────────────────────────────────

#[test]
fn insert_update_delete_walk_the_key() {
    let (_dir, mut f) = open();
    let inserted = affected(
        &mut f.db,
        "INSERT INTO place (_key, key, name, descr, text, born, kind, loc, plot, emb, score, flag) \
         VALUES ($1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11), \
                ($12, $12, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        &[
            Param::Text("z00001".into()),
            Param::Text("kebun baru".into()),
            Param::Text("kopi sawah danau".into()),
            Param::Text("kebun baru kopi sawah danau".into()),
            Param::Int(19_990_101),
            Param::Text("depot".into()),
            Param::Text(r#"{"type":"Point","coordinates":[106.8,-6.2]}"#.into()),
            Param::Text(
                r#"{"type":"Polygon","coordinates":[[[106.7,-6.3],[106.9,-6.3],[106.9,-6.1],[106.7,-6.1],[106.7,-6.3]]]}"#
                    .into(),
            ),
            Param::Vector(fixture::query_vector()),
            Param::Float(12.5),
            Param::Bool(true),
            Param::Text("z00002".into()),
        ],
    );
    assert_eq!(inserted, 2);
    f.db.sql("COMMIT", &[]).unwrap();

    let got = sql_rows(
        &mut f.db,
        "SELECT born FROM place WHERE _key BETWEEN $1 AND $2",
        &[Param::Text("z00001".into()), Param::Text("z00002".into())],
    );
    assert_eq!(got.1.len(), 2);
    assert_eq!(got.1[0][0], SqlValue::Int(19_990_101));

    assert_eq!(
        affected(
            &mut f.db,
            "UPDATE place SET born = $1, kind = $2 WHERE _key = $3",
            &[
                Param::Int(20_000_101),
                Param::Text("park".into()),
                Param::Text("z00001".into()),
            ],
        ),
        1
    );
    f.db.sql("COMMIT", &[]).unwrap();
    let after = f.db.get(f.place, "z00001").unwrap().unwrap();
    assert_eq!(after.document["born"], serde_json::json!(20_000_101));
    assert_eq!(after.document["kind"], serde_json::json!("park"));
    // A partial update keeps the fields it did not name.
    assert_eq!(after.document["name"], serde_json::json!("kebun baru"));

    assert_eq!(
        affected(
            &mut f.db,
            "DELETE FROM place WHERE _key = $1",
            &[Param::Text("z00001".into())]
        ),
        1
    );
    assert_eq!(
        affected(
            &mut f.db,
            "DELETE FROM place WHERE _key = $1",
            &[Param::Text("z00001".into())]
        ),
        0
    );
    f.db.sql("COMMIT", &[]).unwrap();
    assert!(f.db.get(f.place, "z00001").unwrap().is_none());
}

#[test]
fn rollback_discards_an_uncommitted_write() {
    let (_dir, mut f) = open();
    affected(
        &mut f.db,
        "DELETE FROM place WHERE _key = $1",
        &[Param::Text("k00003".into())],
    );
    f.db.sql("ROLLBACK", &[]).unwrap();
    assert!(f.db.get(f.place, "k00003").unwrap().is_some());
}

// ── DDL ───────────────────────────────────────────────────────────────────

#[test]
fn create_table_and_create_index_build_a_queryable_collection() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(
        dir.path().join("db"),
        kernel::store::Config {
            budget_bytes: 8 << 20,
            io: kernel::io::IoMode::Buffered,
            sync: kernel::store::SyncMode::Full,
        },
    )
    .unwrap();

    db.sql(
        "CREATE TABLE town (\
            name TEXT PRIMARY KEY, \
            label TEXT, \
            body TEXT, \
            founded INT, \
            rating DOUBLE PRECISION, \
            active BOOLEAN, \
            props JSONB, \
            at TIMESTAMPTZ, \
            day DATE, \
            loc GEOMETRY(Point,4326), \
            area GEOMETRY(Polygon,4326), \
            emb VECTOR(4))",
        &[],
    )
    .unwrap();
    let town = db.collection("town").unwrap().unwrap();
    let declared = db.collection_info(town).unwrap().layout.fields.clone();
    assert!(declared.iter().any(|(n, k)| n == "emb"
        && matches!(k, e4_prototype::Kind::Vector(4))));
    assert!(declared
        .iter()
        .any(|(n, k)| n == "at" && matches!(k, e4_prototype::Kind::Int)));

    for i in 0..50 {
        db.sql(
            "INSERT INTO town (_key, name, label, body, founded, rating, active, loc, area, emb) \
             VALUES ($1, $1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                Param::Text(format!("t{i:03}")),
                Param::Text(format!("town {i}")),
                Param::Text(format!("kebun {} sawah", fixture::VOCAB[i % 12])),
                Param::Int(1900 + i as i64),
                Param::Float(i as f64 / 2.0),
                Param::Bool(i % 2 == 0),
                Param::Text(format!(
                    r#"{{"type":"Point","coordinates":[{:?},{:?}]}}"#,
                    106.0 + i as f64 * 0.01,
                    -6.0 - i as f64 * 0.01
                )),
                Param::Text(
                    r#"{"type":"Polygon","coordinates":[[[106.0,-6.2],[106.6,-6.2],[106.6,-6.0],[106.0,-6.0],[106.0,-6.2]]]}"#
                        .into(),
                ),
                Param::Vector(vec![1.0, 0.0, 0.0, 0.0]),
            ],
        )
        .unwrap();
    }
    db.sql("COMMIT", &[]).unwrap();

    for ddl in [
        "CREATE INDEX town_founded ON town USING btree (founded)",
        "CREATE INDEX town_body ON town USING gin (to_tsvector('simple', body))",
        "CREATE INDEX town_loc ON town USING gist (loc)",
        "CREATE INDEX town_area ON town USING gist (area)",
        "CREATE INDEX town_emb ON town USING exact (emb)",
        "CREATE INDEX town_emb_ann ON town USING diskann (emb vector_cosine_ops)",
    ] {
        db.sql(ddl, &[]).unwrap();
    }

    let SqlResult::Rows { rows, .. } = db
        .sql(
            "SELECT _key FROM town WHERE founded BETWEEN $1 AND $2",
            &[Param::Int(1900), Param::Int(1909)],
        )
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 10);

    let SqlResult::Rows { rows, .. } = db
        .sql(
            "SELECT _key FROM town WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'kebun')",
            &[],
        )
        .unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 50);

    // Both DROPs have an atomic: the index's is `drop_index_step`, the
    // table's `drop_collection_step`.
    db.sql("DROP INDEX town_founded", &[]).unwrap();
    let after = db
        .sql(
            "SELECT _key FROM town WHERE founded BETWEEN $1 AND $2",
            &[Param::Int(1900), Param::Int(1909)],
        )
        .unwrap_err();
    assert!(format!("{after}").contains("does not exist"), "{after}");

    // Nothing references `town`, so the default RESTRICT drops it, and the
    // collection stops answering.
    match db.sql("DROP TABLE town", &[]).unwrap() {
        SqlResult::Affected(n) => assert!(n >= 50, "{n} entries removed"),
        other => panic!("expected an affected count, got {other:?}"),
    }
    assert!(db.collection("town").unwrap().is_none());
    let gone = db.sql("SELECT _key FROM town LIMIT 1", &[]).unwrap_err();
    assert!(format!("{gone}").contains("no collection named `town`"), "{gone}");
}

// ── GRAPH_TABLE ───────────────────────────────────────────────────────────

#[test]
fn graph_table_compiles_to_one_bounded_traversal() {
    let (_dir, mut f) = open();
    let seed = f.db.get(f.place, "k00000").unwrap().unwrap().id;
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Graph(
            e4_prototype::collections::BfsRequest {
                seed,
                direction: e4_prototype::collections::Direction::Outgoing,
                context: f.context,
                edge_type: Some(f.near),
                min_depth: 1,
                max_depth: 3,
                include_seed: false,
                max_visited: 1 << 16,
                max_edges: 1 << 18,
                result_limit: 1 << 16,
            },
        )],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT k FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[:near]->{1,3}(b:place) \
            COLUMNS (b._key AS k))",
        &[Param::Text("k00000".into())],
    );
    assert_eq!(got, expected);
    assert_eq!(got.len(), 3, "one, two and three hops along the chain");
}

// ── LIMIT ─────────────────────────────────────────────────────────────────

#[test]
fn limit_is_a_total_limit_on_the_prepared_query() {
    let (_dir, mut f) = open();
    let all = sql_ids(&mut f.db, "SELECT _id FROM place WHERE kind = 'port'", &[]);
    let limited = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = 'port' LIMIT 7",
        &[],
    );
    assert_eq!(limited.len(), 7);
    assert_eq!(limited, all[..7]);
}

// ── DROP TABLE ────────────────────────────────────────────────────────────

/// `DROP TABLE [IF EXISTS] name [CASCADE|RESTRICT]`, against the same 2,000-row
/// fixture. The oracle is the direct API: the statement compiles to
/// `begin_drop_collection_mode` and `drop_collection_step` and nothing else,
/// so the questions asked are "does the collection still answer" and "did the
/// refusal say what the contract says it says".
#[test]
fn drop_table_restricts_on_graph_edges_and_cascades_when_asked() {
    let (_dir, mut f) = open();
    // The fixture's every row is an endpoint of a `routes` edge, so RESTRICT
    // refuses and names that context (GRAPH_CONTRACT 6.1).
    let refused = f.db.sql("DROP TABLE place", &[]).unwrap_err().to_string();
    assert!(refused.contains("RESTRICT"), "{refused}");
    assert!(refused.contains("routes"), "{refused}");
    assert!(refused.contains("CASCADE"), "{refused}");
    // A refusal removes nothing.
    assert_eq!(
        f.db.scan(f.place, None).unwrap().count(),
        fixture::ROWS,
        "the refused drop left every row"
    );

    // IF EXISTS on a name that is not there is a notice, not an error.
    match f.db.sql("DROP TABLE IF EXISTS nowhere", &[]).unwrap() {
        SqlResult::Notice(text) => assert!(text.contains("no such collection"), "{text}"),
        other => panic!("expected a notice, got {other:?}"),
    }
    assert!(f
        .db
        .sql("DROP TABLE nowhere", &[])
        .unwrap_err()
        .to_string()
        .contains("nowhere"));

    match f.db.sql("DROP TABLE place CASCADE", &[]).unwrap() {
        // 2,000 rows + 2,000 mappings + 2,000 sidecars is the floor; the
        // indexes, the edges and the descriptors are on top of it.
        SqlResult::Affected(n) => assert!(n >= 6_000, "{n} entries removed"),
        other => panic!("expected an affected count, got {other:?}"),
    }
    assert!(f.db.collection("place").unwrap().is_none());
    assert!(f
        .db
        .sql("SELECT _id FROM place LIMIT 1", &[])
        .unwrap_err()
        .to_string()
        .contains("no collection named `place`"));

    // The name is free again and the identity is not reused.
    f.db.sql("CREATE TABLE place (key TEXT PRIMARY KEY, born INT)", &[])
        .unwrap();
    let again = f.db.collection("place").unwrap().unwrap();
    assert_ne!(again, f.place);
    f.db.sql("INSERT INTO place (key, born) VALUES ('a', 1)", &[])
        .unwrap();
    f.db.sql("COMMIT", &[]).unwrap();
    // The fresh collection has no index, so the question is asked of the
    // external key, which is the key-order driver's own range.
    match f
        .db
        .sql("SELECT born FROM place WHERE _key = 'a'", &[])
        .unwrap()
    {
        SqlResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("expected rows, got {other:?}"),
    }
    // Nothing references it, so the default RESTRICT drops it.
    match f.db.sql("DROP TABLE place RESTRICT", &[]).unwrap() {
        SqlResult::Affected(_) => {}
        other => panic!("expected an affected count, got {other:?}"),
    }
    assert!(f.db.collection("place").unwrap().is_none());
}
