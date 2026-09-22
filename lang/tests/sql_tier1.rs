//! One test per Tier-1 statement family of `docs/lang/QL_CONTRACT.md`, written as
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

use sekejap_lang::SqlDatabase;
use sekejap_core::{
    collections::{
        CandidateDriver, Database, EntityId, Geom, GeometryFilter, PointFilter, Projection,
        QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue, ScoreExpr,
        SortDirection, TextMatch, VectorMetric,
    },
    spatial_math::Bounds,
};
use sekejap_lang::{Param, SqlResult, SqlValue};
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
    collection: sekejap_core::collections::CollectionId,
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
            emb VECTOR(4)) WITH (index: none)",
        &[],
    )
    .unwrap();
    let town = db.collection("town").unwrap().unwrap();
    let declared = db.collection_info(town).unwrap().layout.fields.clone();
    assert!(declared.iter().any(|(n, k)| n == "emb"
        && matches!(k, sekejap_core::Kind::Vector(4))));
    assert!(declared
        .iter()
        .any(|(n, k)| n == "at" && matches!(k, sekejap_core::Kind::Int)));

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
            sekejap_core::collections::BfsRequest {
                seed,
                direction: sekejap_core::collections::Direction::Outgoing,
                context: f.context,
                edge_type: Some(f.near),
                min_depth: 1,
                max_depth: 3,
                include_seed: false,
                max_visited: 1 << 16,
                max_edges: 1 << 18,
                result_limit: 1 << 16,
                edge_where: &[],
                node_where: &[],
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

// ── GROUP BY / HAVING / DISTINCT and the aggregate functions (§4.7) ────────
//
// The direct API is the oracle here too: `Database::prepare_aggregate` is the
// atomic, and these statements' whole claim is that they compile to it.

/// Every group one statement returns, as `(key, values)` in result order.
fn sql_groups(db: &mut Database, text: &str, params: &[Param]) -> Vec<Vec<SqlValue>> {
    match db.sql(text, params).unwrap() {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|row| row.values).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn direct_groups(
    db: &Database,
    collection: sekejap_core::collections::CollectionId,
    filters: &[QueryFilter<'_>],
    group: Option<sekejap_core::collections::GroupKey<'_>>,
    accumulators: &[sekejap_core::collections::Accumulator<'_>],
) -> Vec<sekejap_core::collections::GroupRow> {
    let mut prepared = db
        .prepare_aggregate(sekejap_core::collections::AggregateRequest {
            collection,
            filters,
            group,
            accumulators,
            having: &[],
            order: sekejap_core::collections::GroupOrder::Key,
            driver: CandidateDriver::Auto,
            total_limit: None,
        })
        .unwrap();
    let mut out = Vec::new();
    loop {
        let page = prepared
            .next_page(PAGE, QueryBudget::unlimited(), || false)
            .unwrap();
        let empty = page.groups.is_empty();
        out.extend(page.groups);
        if page.done || empty {
            break;
        }
    }
    out
}

#[test]
fn count_star_with_no_filter_is_one_row_over_the_key_order_driver() {
    let (_dir, mut f) = open();
    let (columns, rows) = sql_rows(&mut f.db, "SELECT count(*) FROM place", &[]);
    assert_eq!(columns, vec!["count".to_owned()]);
    assert_eq!(rows, vec![vec![SqlValue::Int(fixture::ROWS as i64)]]);

    let explained = match f.db.sql("EXPLAIN SELECT count(*) FROM place", &[]).unwrap() {
        SqlResult::Explain(text) => text,
        other => panic!("expected an explanation, got {other:?}"),
    };
    assert!(explained.contains("shape: streaming"), "{explained}");
    assert!(explained.contains("driver: Keys"), "{explained}");
    assert!(explained.contains("primary_reads=0"), "{explained}");
}

#[test]
fn count_star_with_a_filter_counts_what_the_filter_admits() {
    let (_dir, mut f) = open();
    let expected = sql_ids(&mut f.db, "SELECT _id FROM place WHERE kind = 'port'", &[]).len();
    let rows = sql_groups(
        &mut f.db,
        "SELECT count(*) AS n FROM place WHERE kind = $1",
        &[Param::Text("port".into())],
    );
    assert_eq!(rows, vec![vec![SqlValue::Int(expected as i64)]]);
}

#[test]
fn group_by_an_indexed_column_streams_and_matches_the_api() {
    let (_dir, mut f) = open();
    let accumulators = [sekejap_core::collections::Accumulator {
        function: sekejap_core::collections::AggregateFn::CountStar,
        input: None,
    }];
    let expected = direct_groups(
        &f.db,
        f.place,
        &[],
        Some(sekejap_core::collections::GroupKey::Index(f.index.kind)),
        &accumulators,
    );
    let (columns, rows) = sql_rows(
        &mut f.db,
        "SELECT kind, count(*) AS n FROM place GROUP BY kind",
        &[],
    );
    assert_eq!(columns, vec!["kind".to_owned(), "n".to_owned()]);
    assert_eq!(rows.len(), expected.len());
    assert_eq!(rows.len(), fixture::KINDS.len());
    for (got, want) in rows.iter().zip(&expected) {
        let sekejap_core::collections::OwnedScalarValue::Text(key) = want.key.clone().unwrap()
        else {
            panic!("kind is a Text column");
        };
        assert_eq!(got[0], SqlValue::Text(key));
        let sekejap_core::collections::AggValue::Count(n) = want.values[0] else {
            panic!("count(*) is a count");
        };
        assert_eq!(got[1], SqlValue::Int(n as i64));
    }
}

#[test]
fn sum_min_max_avg_by_group_with_having() {
    let (_dir, mut f) = open();
    // `tag` is unindexed and unevenly filled -- the fixture gives `kind`
    // exactly 250 rows each, so no HAVING threshold could split THAT.
    let statement = "SELECT tag, count(*) AS n, sum(born) AS s, min(born) AS lo, max(born) AS hi, \
         avg(born) AS mean FROM place GROUP BY tag";
    let all = sql_groups(&mut f.db, statement, &[]);
    let mut counts: Vec<i64> = all
        .iter()
        .map(|row| match row[1] {
            SqlValue::Int(n) => n,
            _ => panic!("count is a whole number"),
        })
        .collect();
    counts.sort_unstable();
    let threshold = counts[counts.len() / 2];
    let kept = sql_groups(&mut f.db, &format!("{statement} HAVING count(*) > {threshold}"), &[]);
    let expected: Vec<_> = all
        .iter()
        .filter(|row| matches!(row[1], SqlValue::Int(n) if n > threshold))
        .cloned()
        .collect();
    assert_eq!(kept, expected);
    assert!(
        !kept.is_empty() && kept.len() < all.len(),
        "HAVING kept {} of {}",
        kept.len(),
        all.len()
    );
    // The whole is the sum of its parts: every row lands in exactly one
    // group, the missing-`tag` rows included.
    let total: i64 = counts.iter().sum();
    assert_eq!(total, fixture::ROWS as i64);
    // sum, min, max and avg agree with one another on every group.
    for row in &all {
        let (SqlValue::Int(n), SqlValue::Int(sum), SqlValue::Int(lo), SqlValue::Int(hi), SqlValue::Float(mean)) =
            (row[1].clone(), row[2].clone(), row[3].clone(), row[4].clone(), row[5].clone())
        else {
            panic!("unexpected column types in {row:?}");
        };
        assert!(lo <= hi);
        assert!(sum >= lo * n && sum <= hi * n);
        assert!((mean - sum as f64 / n as f64).abs() <= 1e-6, "{mean} against {sum}/{n}");
    }
}

#[test]
fn select_distinct_is_a_group_with_no_accumulators() {
    let (_dir, mut f) = open();
    let grouped = sql_groups(&mut f.db, "SELECT kind FROM place GROUP BY kind", &[]);
    let distinct = sql_groups(&mut f.db, "SELECT DISTINCT kind FROM place", &[]);
    assert_eq!(distinct, grouped);
    assert_eq!(distinct.len(), fixture::KINDS.len());
}

#[test]
fn group_by_with_a_radius_filter_hashes_and_agrees_with_the_filter_itself() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let (lon, lat) = (centre.longitude(), centre.latitude());
    let statement = format!(
        "SELECT kind, count(*) AS n FROM place \
         WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 20000, true) \
         GROUP BY kind"
    );
    let rows = sql_groups(&mut f.db, &statement, &[]);
    let total: i64 = rows
        .iter()
        .map(|row| match row[1] {
            SqlValue::Int(n) => n,
            _ => panic!("count is a whole number"),
        })
        .sum();
    let matching = sql_ids(
        &mut f.db,
        &format!(
            "SELECT _id FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, 20000, true)"
        ),
        &[],
    )
    .len();
    assert_eq!(total, matching as i64);
    assert!(matching > 0, "the radius admits nothing, so this tests nothing");

    let explained = match f.db.sql(&format!("EXPLAIN {statement}"), &[]).unwrap() {
        SqlResult::Explain(text) => text,
        other => panic!("expected an explanation, got {other:?}"),
    };
    assert!(explained.contains("shape: hashed"), "{explained}");
}

#[test]
fn the_divided_group_key_is_accepted_index_side_and_refused_otherwise() {
    let (_dir, mut f) = open();
    let decades = sql_groups(
        &mut f.db,
        "SELECT born / 10000 AS decade, count(*) AS n FROM place GROUP BY born / 10000",
        &[],
    );
    let total: i64 = decades
        .iter()
        .map(|row| match row[1] {
            SqlValue::Int(n) => n,
            _ => panic!("count is a whole number"),
        })
        .sum();
    assert_eq!(total, fixture::ROWS as i64);
    assert!(!decades.is_empty());

    // The same expression over a column with no Int scalar index is REFUSED
    // by name, not folded over rows.
    let refused = f
        .db
        .sql("SELECT count(*) FROM place GROUP BY score / 10", &[])
        .unwrap_err();
    assert!(format!("{refused}").contains("INDEX-SIDE"), "{refused}");
}

#[test]
fn order_by_an_aggregate_alias_sorts_the_finished_groups() {
    let (_dir, mut f) = open();
    let rows = sql_groups(
        &mut f.db,
        "SELECT kind, count(*) AS n FROM place GROUP BY kind ORDER BY n DESC LIMIT 3",
        &[],
    );
    assert_eq!(rows.len(), 3);
    let counts: Vec<i64> = rows
        .iter()
        .map(|row| match row[1] {
            SqlValue::Int(n) => n,
            _ => panic!("count is a whole number"),
        })
        .collect();
    assert!(counts.windows(2).all(|pair| pair[0] >= pair[1]), "{counts:?}");
    let all = sql_groups(&mut f.db, "SELECT kind, count(*) AS n FROM place GROUP BY kind", &[]);
    let mut every: Vec<i64> = all
        .iter()
        .map(|row| match row[1] {
            SqlValue::Int(n) => n,
            _ => panic!("count is a whole number"),
        })
        .collect();
    every.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(counts, every[..3].to_vec());
}

#[test]
fn a_folded_answer_refuses_what_it_cannot_report() {
    let (_dir, mut f) = open();
    for statement in [
        // A column that is neither the key nor an aggregate.
        "SELECT name, count(*) FROM place GROUP BY kind",
        // Two group keys: there is no composite-key atomic.
        "SELECT kind, count(*) FROM place GROUP BY kind, born",
        // A row identity a group does not have.
        "SELECT _id, count(*) FROM place GROUP BY kind",
        // An aggregate over DISTINCT values needs a per-group distinct set.
        "SELECT count(DISTINCT kind) FROM place",
    ] {
        let error = f
            .db
            .sql(statement, &[])
            .expect_err(&format!("`{statement}` was not refused"));
        let shown = format!("{error}");
        assert!(!shown.is_empty(), "`{statement}` refused with no reason");
    }
}

/// The two arms of the `battle50k` aggregate battery ask the SAME question at
/// the SAME cost: the SQL statement and the direct `AggregateRequest` charge
/// identical `QueryWork`.
///
/// This is the claim the SQL layer makes -- it compiles to the calls the crate
/// already has and adds no second engine -- pinned as an equality of counters
/// rather than as a wall-clock comparison, which is a property of the machine
/// as much as of the code.
#[test]
fn the_sql_aggregate_charges_exactly_what_the_api_aggregate_charges() {
    let (_dir, f) = open();
    let count_star = [sekejap_core::collections::Accumulator {
        function: sekejap_core::collections::AggregateFn::CountStar,
        input: None,
    }];
    let cases: Vec<(&str, Option<sekejap_core::collections::GroupKey<'_>>, &str)> = vec![
        ("agg_count_all", None, "SELECT count(*) FROM place"),
        (
            "agg_count_kind",
            Some(sekejap_core::collections::GroupKey::Index(f.index.kind)),
            "SELECT kind, count(*) AS n FROM place GROUP BY kind",
        ),
        (
            "agg_born_decade",
            Some(sekejap_core::collections::GroupKey::IndexDiv {
                index: f.index.born,
                divisor: 10_000,
            }),
            "SELECT born / 10000 AS decade, count(*) AS n FROM place GROUP BY born / 10000",
        ),
    ];
    for (name, group, statement) in cases {
        let mut api = f
            .db
            .prepare_aggregate(sekejap_core::collections::AggregateRequest {
                collection: f.place,
                filters: &[],
                group,
                accumulators: &count_star,
                having: &[],
                order: sekejap_core::collections::GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            })
            .unwrap();
        let mut api_work = sekejap_core::collections::QueryWork::default();
        let mut api_groups = 0usize;
        loop {
            let page = api
                .next_page(PAGE, QueryBudget::unlimited(), || false)
                .unwrap();
            api_work.candidates += page.work.candidates;
            api_work.primary_reads += page.work.primary_reads;
            api_work.scalar_postings += page.work.scalar_postings;
            api_work.key_postings += page.work.key_postings;
            api_groups += page.groups.len();
            if page.done || page.groups.is_empty() {
                break;
            }
        }

        let prepared = sekejap_lang::prepare_sql(&f.db, statement, &[]).unwrap();
        let (sql_work, sql_groups) = prepared
            .with_aggregate(&f.db, &mut |aggregate| {
                let mut work = sekejap_core::collections::QueryWork::default();
                let mut groups = 0usize;
                loop {
                    let page = aggregate.next_page(PAGE, QueryBudget::unlimited(), || false)?;
                    work.candidates += page.work.candidates;
                    work.primary_reads += page.work.primary_reads;
                    work.scalar_postings += page.work.scalar_postings;
                    work.key_postings += page.work.key_postings;
                    groups += page.groups.len();
                    if page.done || page.groups.is_empty() {
                        break;
                    }
                }
                Ok((work, groups))
            })
            .unwrap();

        assert_eq!(api_groups, sql_groups, "{name}: group count");
        assert_eq!(
            api_work.candidates, sql_work.candidates,
            "{name}: candidates"
        );
        assert_eq!(
            api_work.primary_reads, sql_work.primary_reads,
            "{name}: primary_reads"
        );
        assert_eq!(
            api_work.scalar_postings, sql_work.scalar_postings,
            "{name}: scalar_postings"
        );
        assert_eq!(
            api_work.key_postings, sql_work.key_postings,
            "{name}: key_postings"
        );
    }
}

// ── GRAPH_TABLE: per-hop predicates (GRAPH_CONTRACT 4.2 and 4.3) ──────────

/// A second edge type over the same rows, carrying properties, so the
/// per-hop statements below have a bag to read. The fixture's own `near`
/// edges carry none; this adds `weighted` rather than changing them, because
/// every other suite over that fixture reads the same edges.
fn weighted_graph(f: &mut fixture::Fixture) -> sekejap_core::collections::EdgeTypeId {
    let weighted = f.db.create_edge_type("weighted").unwrap();
    f.db.commit().unwrap();
    let ids: Vec<EntityId> = f
        .keys
        .iter()
        .map(|key| f.db.get(f.place, key).unwrap().unwrap().id)
        .collect();
    // A chain of 1,999 edges whose weight rises along it, so `r.weight > x`
    // has a known cut and a node's `born` has a known one too.
    for (i, pair) in ids.windows(2).enumerate() {
        let weight = (i % 10) as f64 / 10.0;
        f.db.put_edge(
            f.context,
            pair[0],
            weighted,
            pair[1],
            &serde_json::json!({"weight": weight, "rank": (i % 7) as i64}),
        )
        .unwrap();
        if (i + 1) % 256 == 0 {
            f.db.commit().unwrap();
        }
    }
    f.db.commit().unwrap();
    weighted
}

#[test]
fn graph_table_inline_edge_where_compiles_to_a_per_hop_prune() {
    let (_dir, mut f) = open();
    let weighted = weighted_graph(&mut f);
    let seed = f.db.get(f.place, "k00000").unwrap().unwrap().id;
    let edge_where = [sekejap_core::collections::EdgePredicate {
        property: "weight",
        op: sekejap_core::collections::Cmp::Gt,
        value: sekejap_core::collections::ScalarValue::F64(0.2),
    }];
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Graph(
            sekejap_core::collections::BfsRequest {
                seed,
                direction: sekejap_core::collections::Direction::Outgoing,
                context: f.context,
                edge_type: Some(weighted),
                min_depth: 1,
                max_depth: 4,
                include_seed: false,
                max_visited: 1 << 16,
                max_edges: 1 << 18,
                result_limit: 1 << 16,
                edge_where: &edge_where,
                node_where: &[],
            },
        )],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT k FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[r:weighted WHERE r.weight > 0.2]->{1,4}(b:place) \
            COLUMNS (b._key AS k))",
        &[Param::Text("k00000".into())],
    );
    assert_eq!(got, expected);
    // The chain's first edge has weight 0.0, so the prune stops it dead and
    // the unpruned pattern does not.
    assert!(got.is_empty(), "weight 0.0 on the first hop is not > 0.2");
    let unpruned = sql_ids(
        &mut f.db,
        "SELECT k FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[r:weighted]->{1,4}(b:place) \
            COLUMNS (b._key AS k))",
        &[Param::Text("k00000".into())],
    );
    assert_eq!(unpruned.len(), 4, "four hops along the chain");
    // From `k00003` the chain's weights are 0.3, 0.4, 0.5, 0.6, so the same
    // predicate admits every hop: the prune is about the EDGE, not about the
    // pattern.
    let admitted = sql_ids(
        &mut f.db,
        "SELECT k FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[r:weighted WHERE r.weight > 0.2]->{1,4}(b:place) \
            COLUMNS (b._key AS k))",
        &[Param::Text("k00003".into())],
    );
    assert_eq!(admitted.len(), 4);
    // And a predicate that cuts mid-chain stops it there: from `k00003` the
    // third hop is weight 0.5, so `< 0.5` returns two rows.
    let cut = sql_ids(
        &mut f.db,
        "SELECT k FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[r:weighted WHERE r.weight < 0.5]->{1,4}(b:place) \
            COLUMNS (b._key AS k))",
        &[Param::Text("k00003".into())],
    );
    assert_eq!(cut.len(), 2, "the chain stops at the first refused edge");
}

#[test]
fn graph_table_inline_node_where_compiles_to_a_membership_prune() {
    let (_dir, mut f) = open();
    let weighted = weighted_graph(&mut f);
    let seed = f.db.get(f.place, "k00000").unwrap().unwrap().id;
    let node_where = [QueryFilter::Scalar {
        index: f.index.born,
        predicate: sekejap_core::collections::ScalarFilter::Range {
            lower: std::ops::Bound::Included(sekejap_core::collections::ScalarValue::I64(
                19_500_101,
            )),
            upper: std::ops::Bound::Included(sekejap_core::collections::ScalarValue::I64(
                19_600_101,
            )),
        },
    }];
    let expected = direct(
        &f.db,
        f.place,
        &[QueryFilter::Graph(
            sekejap_core::collections::BfsRequest {
                seed,
                direction: sekejap_core::collections::Direction::Outgoing,
                context: f.context,
                edge_type: Some(weighted),
                min_depth: 1,
                max_depth: 4,
                include_seed: false,
                max_visited: 1 << 16,
                max_edges: 1 << 18,
                result_limit: 1 << 16,
                edge_where: &[],
                node_where: &node_where,
            },
        )],
        QueryOrder::Driver,
        None,
    );
    let got = sql_ids(
        &mut f.db,
        "SELECT k FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[r:weighted]->{1,4}\
            (b:place WHERE b.born BETWEEN 19500101 AND 19600101) \
            COLUMNS (b._key AS k))",
        &[Param::Text("k00000".into())],
    );
    assert_eq!(got, expected);
}

#[test]
fn graph_table_columns_project_the_reaching_edge_and_order_by_it() {
    let (_dir, mut f) = open();
    let _ = weighted_graph(&mut f);
    let (columns, rows) = sql_rows(
        &mut f.db,
        "SELECT k, w FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[r:weighted]->{1,6}(b:place) \
            COLUMNS (b._key AS k, r.weight AS w)) \
         ORDER BY w DESC LIMIT 3",
        &[Param::Text("k00000".into())],
    );
    assert_eq!(columns, vec!["k".to_owned(), "w".to_owned()]);
    assert_eq!(rows.len(), 3);
    let weights: Vec<f64> = rows
        .iter()
        .map(|row| match &row[1] {
            SqlValue::Float(value) => *value,
            other => panic!("edge weight came back as {other:?}"),
        })
        .collect();
    assert!(
        weights.windows(2).all(|pair| pair[0] >= pair[1]),
        "not descending: {weights:?}"
    );
    // The chain from k00000 crosses weights 0.0 .. 0.5 in six hops, so the
    // top three are 0.5, 0.4 and 0.3.
    assert_eq!(weights, vec![0.5, 0.4, 0.3]);
}

#[test]
fn a_row_bound_inline_node_predicate_is_refused_with_its_tier() {
    let (_dir, mut f) = open();
    let _ = weighted_graph(&mut f);
    for (sql, needle) in [
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH \
                (a:place WHERE a._key = 'k00000')-[r:weighted]->(b:place WHERE b.born IS NULL) \
                COLUMNS (b._key AS k))",
            "nullish index key",
        ),
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH \
                (a:place WHERE a._key = 'k00000')-[r:weighted]->\
                (b:place WHERE to_tsvector('simple', b.text) @@ to_tsquery('simple', 'harbour')) \
                COLUMNS (b._key AS k))",
            "index postings",
        ),
    ] {
        let error = f.db.sql(sql, &[]).unwrap_err();
        let text = format!("{error}");
        assert!(text.contains("refused"), "{text}");
        assert!(text.contains(needle), "{text}");
    }
}

/// D1 in the SQL surface: a `_key` predicate beside a `GRAPH_TABLE` that
/// reads the reaching edge is a POST-FILTER, so the traversal keeps the
/// driver (`GRAPH_CONTRACT 4.2`: the edge is carried by the walk that crossed
/// it and by nothing else) and the key range is answered from the external
/// key the row itself carries. The statement used to compile onto the keys
/// driver, where every edge column was `Missing` and the ranking a total tie.
#[test]
fn a_key_post_filter_beside_a_graph_table_keeps_the_traversal_driving() {
    let (_dir, mut f) = open();
    let _ = weighted_graph(&mut f);
    let (columns, rows) = sql_rows(
        &mut f.db,
        "SELECT k, w FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = $1)-[r:weighted]->{1,6}(b:place) \
            COLUMNS (b._key AS k, r.weight AS w)) \
         WHERE _key <= 'k00004' ORDER BY w DESC",
        &[Param::Text("k00000".into())],
    );
    assert_eq!(columns, vec!["k".to_owned(), "w".to_owned()]);
    // The chain leaves k00000 for k00001..k00006 with weights 0.0..0.5; the
    // key range keeps the first four of them.
    let keys: Vec<String> = rows
        .iter()
        .map(|row| match &row[0] {
            SqlValue::Text(value) => value.clone(),
            other => panic!("key came back as {other:?}"),
        })
        .collect();
    assert_eq!(keys, vec!["k00004", "k00003", "k00002", "k00001"]);
    let weights: Vec<f64> = rows
        .iter()
        .map(|row| match &row[1] {
            SqlValue::Float(value) => *value,
            other => panic!("edge weight came back as {other:?}, not the edge's own"),
        })
        .collect();
    assert_eq!(weights, vec![0.3, 0.2, 0.1, 0.0]);
}

/// D6: an edge alias in `COLUMNS` is matched the way every other name in this
/// parser is -- without case. `R.weight` against a pattern that bound `r` is
/// the EDGE's property, not a row field of the far node that does not exist.
#[test]
fn an_edge_alias_in_columns_is_matched_without_case() {
    let (_dir, mut f) = open();
    let _ = weighted_graph(&mut f);
    let lower = sql_rows(
        &mut f.db,
        "SELECT w FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = 'k00000')-[r:weighted]->{1,6}(b:place) \
            COLUMNS (r.weight AS w)) \
         ORDER BY w DESC LIMIT 3",
        &[],
    );
    let upper = sql_rows(
        &mut f.db,
        "SELECT w FROM GRAPH_TABLE (routes MATCH \
            (a:place WHERE a._key = 'k00000')-[r:weighted]->{1,6}(b:place) \
            COLUMNS (R.weight AS w)) \
         ORDER BY w DESC LIMIT 3",
        &[],
    );
    assert_eq!(lower, upper);
    assert_eq!(
        lower.1,
        vec![
            vec![SqlValue::Float(0.5)],
            vec![SqlValue::Float(0.4)],
            vec![SqlValue::Float(0.3)],
        ]
    );
}

/// D5: an ANONYMOUS edge element bound no variable, so a qualified name in
/// its inline WHERE names something else. Compiling it as an edge property
/// would test the bag for a key it does not carry and return no rows at all.
/// D7: a three-part name inside `GRAPH_TABLE` is a syntax error naming that
/// construct, not a `CREATE SCHEMA` refusal.
#[test]
fn a_graph_table_refuses_a_name_that_belongs_to_another_element() {
    let (_dir, mut f) = open();
    let _ = weighted_graph(&mut f);
    for (sql, needle) in [
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH \
                (a:place WHERE a._key = 'k00000')-[:weighted WHERE b.born > 1990]->(b:place) \
                COLUMNS (b._key AS k))",
            "bound no variable",
        ),
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH \
                (a:place WHERE a._key = 'k00000')-[r:weighted WHERE b.born > 1990]->(b:place) \
                COLUMNS (b._key AS k))",
            "names its own element",
        ),
        (
            "SELECT k FROM GRAPH_TABLE (routes MATCH \
                (a:place WHERE a._key = 'k00000')-[r:weighted]->(b:place) \
                COLUMNS (x.y.z AS k))",
            "GRAPH_TABLE element name",
        ),
    ] {
        let error = f.db.sql(sql, &[]).unwrap_err();
        let text = format!("{error}");
        assert!(text.contains(needle), "want `{needle}`, got: {text}");
        assert!(
            !text.contains("CREATE SCHEMA"),
            "a GRAPH_TABLE name was refused as a schema qualifier: {text}"
        );
    }
}

// ── §3 boolean predicates: OR, IN, NOT, `<>`, IS NOT NULL, EXISTS ────────
//
// The oracle stays what it is everywhere else in this file: the rows the
// direct API returns for the same question, with the boolean tree written
// through `QueryFilter::Any`/`All`/`Not` on the stack.

/// Build a borrowed disjunction of two leaves and hand it to `k`, the way
/// `lang/src/compile.rs` builds one.
fn with_union<R>(
    left: QueryFilter<'_>,
    right: QueryFilter<'_>,
    k: &mut dyn FnMut(&QueryFilter<'_>) -> R,
) -> R {
    let leaves = [left, right];
    k(&QueryFilter::Any(&leaves))
}

#[test]
fn a_disjunction_of_equalities_is_one_membership_set() {
    let (_dir, mut f) = open();
    let sql_rows_ids = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = $1 OR kind = $2 ORDER BY _id",
        &[Param::Text("depot".into()), Param::Text("mill".into())],
    );
    let want = with_union(
        QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text("depot")),
        },
        QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text("mill")),
        },
        &mut |filter| direct(&f.db, f.place, &[filter.clone()], QueryOrder::EntityId, None),
    );
    assert!(!want.is_empty());
    assert_eq!(sql_rows_ids, want);
}

#[test]
fn in_a_list_is_the_same_union_written_shorter() {
    let (_dir, mut f) = open();
    let listed = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind IN ($1, $2, $3) ORDER BY _id",
        &[
            Param::Text("depot".into()),
            Param::Text("mill".into()),
            Param::Text("port".into()),
        ],
    );
    let written_out = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = $1 OR kind = $2 OR kind = $3 ORDER BY _id",
        &[
            Param::Text("depot".into()),
            Param::Text("mill".into()),
            Param::Text("port".into()),
        ],
    );
    assert_eq!(listed, written_out);
    assert!(!listed.is_empty());
}

#[test]
fn a_parenthesised_group_binds_the_way_sql_says() {
    let (_dir, mut f) = open();
    let grouped = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE flag = true AND (kind = $1 OR kind = $2) ORDER BY _id",
        &[Param::Text("depot".into()), Param::Text("mill".into())],
    );
    let ungrouped = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE flag = true AND kind = $1 OR kind = $2 ORDER BY _id",
        &[Param::Text("depot".into()), Param::Text("mill".into())],
    );
    assert_ne!(
        grouped, ungrouped,
        "AND binds tighter than OR, so the parentheses change the answer"
    );
    let want = with_union(
        QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text("depot")),
        },
        QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text("mill")),
        },
        &mut |union| {
            direct(
                &f.db,
                f.place,
                &[
                    QueryFilter::Scalar {
                        index: f.index.flag,
                        predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
                    },
                    union.clone(),
                ],
                QueryOrder::EntityId,
                None,
            )
        },
    );
    assert_eq!(grouped, want);
}

#[test]
fn not_equal_is_the_complement_of_an_equality() {
    let (_dir, mut f) = open();
    let angle = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind <> $1 ORDER BY _id",
        &[Param::Text("depot".into())],
    );
    let bang = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind != $1 ORDER BY _id",
        &[Param::Text("depot".into())],
    );
    assert_eq!(angle, bang, "`<>` and `!=` are one operator");
    let eq = QueryFilter::Scalar {
        index: f.index.kind,
        predicate: ScalarFilter::Eq(ScalarValue::Text("depot")),
    };
    let want = direct(
        &f.db,
        f.place,
        &[QueryFilter::Not(&eq)],
        QueryOrder::EntityId,
        None,
    );
    assert_eq!(angle, want);
    let kept = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind = $1 ORDER BY _id",
        &[Param::Text("depot".into())],
    );
    assert_eq!(
        angle.len() + kept.len(),
        fixture::ROWS,
        "every row has a kind, so the two halves are the whole collection"
    );
}

#[test]
fn a_null_value_is_in_neither_half_of_a_complement() {
    let (_dir, mut f) = open();
    // One row in seventeen has a JSON null score; `score <> 12.5` is UNKNOWN
    // for those, so they are in neither answer, exactly as in SQL.
    let below_or_above = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE score <> $1 ORDER BY _id",
        &[Param::Float(12.5)],
    );
    let equal = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE score = $1 ORDER BY _id",
        &[Param::Float(12.5)],
    );
    let nulls = f.rows.iter().filter(|row| row.score.is_none()).count();
    assert!(nulls > 0);
    assert_eq!(below_or_above.len() + equal.len() + nulls, fixture::ROWS);
}

#[test]
fn is_not_null_is_the_complement_of_the_nullish_key() {
    let (_dir, mut f) = open();
    let present = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE score IS NOT NULL ORDER BY _id",
        &[],
    );
    let absent = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE score IS NULL ORDER BY _id",
        &[],
    );
    assert_eq!(present.len() + absent.len(), fixture::ROWS);
    let want: Vec<EntityId> = f
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.score.is_some())
        .map(|(at, _)| EntityId {
            collection: f.place,
            sequence: (at + 1) as u64,
        })
        .collect();
    assert_eq!(present, want);
}

#[test]
fn not_before_a_group_is_de_morgan() {
    let (_dir, mut f) = open();
    let negated = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE NOT (kind = $1 OR kind = $2) ORDER BY _id",
        &[Param::Text("depot".into()), Param::Text("mill".into())],
    );
    let spelled_out = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind <> $1 AND kind <> $2 ORDER BY _id",
        &[Param::Text("depot".into()), Param::Text("mill".into())],
    );
    assert_eq!(negated, spelled_out);
    assert_eq!(negated.len(), fixture::ROWS - 2 * fixture::ROWS / 8);
}

#[test]
fn not_in_a_list_is_the_complement_of_the_union() {
    let (_dir, mut f) = open();
    let excluded = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind NOT IN ($1, $2) ORDER BY _id",
        &[Param::Text("depot".into()), Param::Text("mill".into())],
    );
    let included = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE kind IN ($1, $2) ORDER BY _id",
        &[Param::Text("depot".into()), Param::Text("mill".into())],
    );
    assert_eq!(excluded.len() + included.len(), fixture::ROWS);
    assert!(excluded.iter().all(|id| !included.contains(id)));
}

#[test]
fn a_disjunction_across_two_families_unions_two_sets() {
    let (_dir, mut f) = open();
    let mixed = sql_ids(
        &mut f.db,
        "SELECT _id FROM place \
         WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) OR born < $2 \
         ORDER BY _id",
        &[Param::Text("kebun".into()), Param::Int(19_520_101)],
    );
    let want = with_union(
        QueryFilter::Text {
            index: f.index.text,
            query: "kebun",
            matching: TextMatch::Any,
        },
        QueryFilter::Scalar {
            index: f.index.born,
            predicate: ScalarFilter::Range {
                lower: Bound::Unbounded,
                upper: Bound::Excluded(ScalarValue::I64(19_520_101)),
            },
        },
        &mut |filter| direct(&f.db, f.place, &[filter.clone()], QueryOrder::EntityId, None),
    );
    assert!(!want.is_empty());
    assert_eq!(mixed, want);
}

#[test]
fn a_disjunction_with_a_geometry_leaf_is_refused_with_its_reason() {
    let (_dir, mut f) = open();
    let error = f
        .db
        .sql(
            "SELECT _id FROM place \
             WHERE kind = 'depot' OR ST_DWithin(plot, ST_MakePoint(106.82, -6.17)::geography, 500)",
            &[],
        )
        .unwrap_err();
    let text = format!("{error}");
    assert!(
        text.contains("geometry posting's box is a candidate test"),
        "the refusal names the missing set: {text}"
    );
}

/// `FROM <edge type>` inside a subquery names the BASE graph
/// (`GRAPH_CONTRACT` 3.1: "no context means the base graph"), so the edges
/// this test asks about are written there. The fixture's own `near` edges are
/// in the named `routes` context and are a different graph.
#[test]
fn exists_over_an_edge_type_is_a_semi_join() {
    let (_dir, mut f) = open();
    let linked = f.db.create_edge_type("linked").unwrap();
    f.db.commit().unwrap();
    let sources: Vec<usize> = (0..fixture::ROWS).filter(|at| at % 4 == 1).collect();
    for at in &sources {
        let from = f.db.get(f.place, &f.rows[*at].key).unwrap().unwrap().id;
        let to = f
            .db
            .get(f.place, &f.rows[(at + 1) % fixture::ROWS].key)
            .unwrap()
            .unwrap()
            .id;
        f.db.put_edge(
            sekejap_core::collections::GraphContextId::BASE,
            from,
            linked,
            to,
            &serde_json::json!({}),
        )
        .unwrap();
    }
    f.db.commit().unwrap();

    let want: Vec<EntityId> = sources
        .iter()
        .map(|at| EntityId {
            collection: f.place,
            sequence: (*at + 1) as u64,
        })
        .collect();
    let with_edges = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE EXISTS (SELECT 1 FROM linked WHERE source = _key) \
         ORDER BY _id",
        &[],
    );
    assert_eq!(with_edges, want);
    let without = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE NOT EXISTS (SELECT 1 FROM linked WHERE source = _key) \
         ORDER BY _id",
        &[],
    );
    assert_eq!(with_edges.len() + without.len(), fixture::ROWS);
    assert!(without.iter().all(|id| !with_edges.contains(id)));
    let same = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE _key IN (SELECT source FROM linked) ORDER BY _id",
        &[],
    );
    assert_eq!(same, with_edges);
}

/// Parentheses that change nothing about the MEANING change nothing about
/// the plan (`docs/lang/QL_CONTRACT.md` §3).
///
/// `where_clause` flattened only the top `AND`, so `a AND (b AND geometry)`
/// left a nested conjunction whose geometry leaf `compile_set_expr` then
/// refused as a boolean leaf -- while the identical statement written without
/// the redundant parentheses compiled and ran.
#[test]
fn redundant_parentheses_do_not_change_what_compiles() {
    let (_dir, mut f) = open();
    let centre = fixture::centre();
    let (lon, lat) = (centre.longitude(), centre.latitude());
    let geometry = format!(
        "ST_DWithin(plot, ST_SetSRID(ST_MakePoint({lon:?}, {lat:?}),4326)::geography, 20000)"
    );
    let flat = format!(
        "SELECT _id FROM place WHERE kind = 'depot' AND born > 19500101 AND {geometry} ORDER BY _id"
    );
    let nested = format!(
        "SELECT _id FROM place WHERE kind = 'depot' AND (born > 19500101 AND {geometry}) ORDER BY _id"
    );
    let deeper = format!(
        "SELECT _id FROM place WHERE (kind = 'depot' AND (born > 19500101)) AND ({geometry}) ORDER BY _id"
    );
    let want = sql_ids(&mut f.db, &flat, &[]);
    assert!(!want.is_empty(), "the flat form answers something");
    assert_eq!(sql_ids(&mut f.db, &nested, &[]), want, "{nested}");
    assert_eq!(sql_ids(&mut f.db, &deeper, &[]), want, "{deeper}");
}

/// A semi-join names outer rows by their EXTERNAL KEY, which is text. A
/// projected column of any other type used to drop every row of the subquery
/// silently and answer with the empty set.
#[test]
fn a_semi_join_over_a_non_text_column_is_refused_naming_it() {
    let (_dir, mut f) = open();
    let error = f
        .db
        .sql("SELECT _id FROM place WHERE _key IN (SELECT born FROM place)", &[])
        .err()
        .expect("a numeric projected column is refused");
    let text = format!("{error}");
    assert!(text.contains("born"), "the refusal names the column: {text}");
    assert!(text.contains("a number"), "and says what it holds: {text}");

    // The text column still works, and names every row.
    let all = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE _key IN (SELECT key FROM place) ORDER BY _id",
        &[],
    );
    assert_eq!(all.len(), fixture::ROWS);
}

/// The semi-join set is built while the statement COMPILES, so the caller's
/// cancellation has to reach it there -- `Database::sql` used to hand the
/// inner walk a closure that always said no.
#[test]
fn a_semi_join_is_cancellable_while_it_compiles() {
    let (_dir, mut f) = open();
    let mut calls = 0usize;
    let error = f
        .db
        .sql_with(
            "SELECT _id FROM place WHERE _key IN (SELECT key FROM place)",
            &[],
            sekejap_core::collections::QueryBudget::unlimited(),
            &mut || {
                calls += 1;
                true
            },
        )
        .err()
        .expect("the inner walk is cancellable");
    assert!(
        format!("{error:?}").contains("Cancelled"),
        "{error:?} after {calls} polls"
    );
    assert!(calls > 0, "the cancel closure was asked");

    // And the caller's budget bounds it: the inner scan reads rows.
    let mut budget = sekejap_core::collections::QueryBudget::unlimited();
    budget.primary_reads = 4;
    let error = f
        .db
        .sql_with(
            "SELECT _id FROM place WHERE _key IN (SELECT key FROM place)",
            &[],
            budget,
            &mut || false,
        )
        .err()
        .expect("the inner walk is bounded");
    assert!(
        format!("{error:?}").contains("BudgetExceeded"),
        "{error:?}"
    );
}

#[test]
fn a_negated_tsquery_is_the_complement_of_the_text_set() {
    let (_dir, mut f) = open();
    let without = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) \
         ORDER BY _id",
        &[Param::Text("!kebun".into())],
    );
    let with = sql_ids(
        &mut f.db,
        "SELECT _id FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) \
         ORDER BY _id",
        &[Param::Text("kebun".into())],
    );
    assert!(!with.is_empty() && !without.is_empty());
    assert_eq!(with.len() + without.len(), fixture::ROWS);
    assert!(without.iter().all(|id| !with.contains(id)));
}
// ── §4.1 string and §4.2 date/time functions ──────────────────────────────
//
// The fixture's `place` collection has no declared TIMESTAMPTZ column, so the
// date/time rows below build a small side collection through the SQL DDL --
// which is also what proves the declared type reaches the catalog descriptor
// from `CREATE TABLE` and comes back out of it. The string rows ride the
// fixture's own `place`, whose `kind` and `name` are ordinary TEXT columns.

/// `CREATE TABLE ... TIMESTAMPTZ` over the same database, with the indexes
/// the §4.2 rewrites name. Returns the keys, in insert order.
fn timeline(db: &mut Database) -> Vec<(String, &'static str)> {
    db.sql(
        "CREATE TABLE evt (k TEXT PRIMARY KEY, kind TEXT, at TIMESTAMPTZ, on_day DATE)",
        &[],
    )
    .unwrap();
    let rows: Vec<(String, &'static str)> = vec![
        ("e1".to_owned(), "1950-03-04T05:06:07Z"),
        ("e2".to_owned(), "1950-12-31T23:59:59Z"),
        ("e3".to_owned(), "1951-01-01T00:00:00Z"),
        ("e4".to_owned(), "1962-06-15T12:30:00Z"),
    ];
    for (n, (key, at)) in rows.iter().enumerate() {
        db.sql(
            &format!(
                "INSERT INTO evt (k, kind, at, on_day) VALUES ('{key}', '{}', '{at}', '{}')",
                ["Home", "home", "HOME", "Farm"][n],
                &at[..10]
            ),
            &[],
        )
        .unwrap();
    }
    db.commit().unwrap();
    db.sql("CREATE INDEX evt_at ON evt USING btree (at)", &[])
        .unwrap();
    db.sql("CREATE INDEX evt_day ON evt USING btree (on_day)", &[])
        .unwrap();
    db.sql("CREATE INDEX evt_kind ON evt USING btree (kind)", &[])
        .unwrap();
    db.sql("CREATE INDEX evt_kind_lower ON evt (lower(kind))", &[])
        .unwrap();
    db.commit().unwrap();
    rows
}

fn evt_keys(f: &mut fixture::Fixture, sql: &str) -> Vec<String> {
    let mut out = match f.db.sql(sql, &[]).unwrap() {
        SqlResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| match &row.values[0] {
                SqlValue::Text(text) => text.clone(),
                other => panic!("expected a key, got {other:?}"),
            })
            .collect::<Vec<_>>(),
        other => panic!("expected rows, got {other:?}"),
    };
    out.sort();
    out
}

/// §4.2: `EXTRACT(YEAR FROM t)`, `date_trunc`, `t::date`, `t >= lit` and
/// `t BETWEEN` each fold into ONE scalar range on the column's own index.
#[test]
fn a_date_time_function_in_where_is_one_scalar_range() {
    let (_dir, mut f) = open();
    timeline(&mut f.db);
    for (sql, want) in [
        (
            "SELECT k FROM evt WHERE EXTRACT(YEAR FROM at) = 1950",
            vec!["e1", "e2"],
        ),
        (
            "SELECT k FROM evt WHERE EXTRACT(YEAR FROM at) BETWEEN 1951 AND 1962",
            vec!["e3", "e4"],
        ),
        (
            "SELECT k FROM evt WHERE date_trunc('year', at) = '1950-01-01'",
            vec!["e1", "e2"],
        ),
        (
            "SELECT k FROM evt WHERE date_trunc('month', at) BETWEEN '1950-01-01' AND '1950-06-01'",
            vec!["e1"],
        ),
        (
            "SELECT k FROM evt WHERE at::date = '1951-01-01'",
            vec!["e3"],
        ),
        (
            "SELECT k FROM evt WHERE on_day = '1962-06-15'",
            vec!["e4"],
        ),
        ("SELECT k FROM evt WHERE at >= '1951-01-01'", vec!["e3", "e4"]),
        (
            "SELECT k FROM evt WHERE at BETWEEN '1950-01-01' AND '1951-01-01'",
            vec!["e1", "e2", "e3"],
        ),
        (
            "SELECT k FROM evt WHERE at > now() - interval '7 days'",
            vec![],
        ),
    ] {
        assert_eq!(evt_keys(&mut f, sql), want, "{sql}");
    }
}

/// §4.2: the same functions in a SELECT list are ROW functions, and a
/// declared TIMESTAMPTZ prints back as the ISO string it was written as.
#[test]
fn a_date_time_function_in_a_select_list_is_a_row_function() {
    let (_dir, mut f) = open();
    timeline(&mut f.db);
    let rows = match f
        .db
        .sql(
            "SELECT k, at, on_day, EXTRACT(YEAR FROM at), EXTRACT(MONTH FROM at), \
             to_char(at, 'YYYY-MM'), to_char(at, 'HH24:MI'), date_trunc('day', at) \
             FROM evt WHERE at::date = '1962-06-15'",
            &[],
        )
        .unwrap()
    {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(rows.len(), 1);
    let values = &rows[0].values;
    assert_eq!(values[0], SqlValue::Text("e4".into()));
    assert_eq!(values[1], SqlValue::Text("1962-06-15T12:30:00Z".into()));
    assert_eq!(values[2], SqlValue::Text("1962-06-15".into()));
    assert_eq!(values[3], SqlValue::Int(1962));
    assert_eq!(values[4], SqlValue::Int(6));
    assert_eq!(values[5], SqlValue::Text("1962-06".into()));
    assert_eq!(values[6], SqlValue::Text("12:30".into()));
    // `date_trunc` in a projection keeps the STORED representation: an
    // integer of microseconds, which is midnight UTC of that day. Days from
    // 1970-01-01 to 1962-06-15 is -2757.
    assert_eq!(values[7], SqlValue::Int(-2757 * 86_400_000_000));
}

/// §4.1: `lower(col) = x` rides the expression index, `col LIKE 'x%'` and
/// `starts_with` ride the ordinary text key, and each answers the same
/// question the direct API answers with the range it compiles to.
#[test]
fn a_string_function_in_where_is_a_text_key_range() {
    let (_dir, mut f) = open();
    timeline(&mut f.db);
    assert_eq!(
        evt_keys(&mut f, "SELECT k FROM evt WHERE lower(kind) = 'home'"),
        vec!["e1", "e2", "e3"]
    );
    assert_eq!(
        evt_keys(&mut f, "SELECT k FROM evt WHERE kind = 'home'"),
        vec!["e2"]
    );
    assert_eq!(
        evt_keys(&mut f, "SELECT k FROM evt WHERE lower(kind) LIKE 'ho%'"),
        vec!["e1", "e2", "e3"]
    );

    // On the fixture's own collection, over its `kind` btree. The oracle is
    // a BRUTE-FORCE filter over the rows this process holds, not the
    // production range walk over the same two bounds: asking the engine for
    // the bounds it would have computed makes the test agree with a wrong
    // prefix successor (`functions::prefix_successor`) as readily as with a
    // right one, which is exactly the thing under test here.
    let mut want: Vec<EntityId> = f
        .rows
        .iter()
        .filter(|row| row.kind.starts_with("ho"))
        .map(|row| f.db.get(f.place, &row.key).unwrap().unwrap().id)
        .collect();
    want.sort_by_key(|id| id.sequence);
    assert!(!want.is_empty(), "the fixture holds `home` rows");
    assert!(
        want.len() < f.rows.len(),
        "and rows the prefix must NOT admit"
    );
    for statement in [
        "SELECT _id FROM place WHERE kind LIKE 'ho%'",
        "SELECT _id FROM place WHERE starts_with(kind, 'ho')",
    ] {
        let mut got = sql_ids(&mut f.db, statement, &[]);
        got.sort_by_key(|id| id.sequence);
        assert_eq!(got, want, "{statement}");
    }
    // The same range walked by the direct API, kept as the second reading:
    // the SQL layer compiles to the call a caller would have written, and
    // both agree with the brute-force answer.
    let mut api = direct(
        &f.db,
        f.place,
        &[QueryFilter::Scalar {
            index: f.index.kind,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::Text("ho")),
                upper: Bound::Excluded(ScalarValue::Text("hp")),
            },
        }],
        QueryOrder::Driver,
        None,
    );
    api.sort_by_key(|id| id.sequence);
    assert_eq!(api, want);
}

/// §4.1: every string function the contract names, over projected values,
/// against Rust's own computation on the same row.
#[test]
fn a_string_function_in_a_select_list_is_a_row_function() {
    let (_dir, mut f) = open();
    let row = f.rows[3].clone();
    let rows = match f
        .db
        .sql(
            &format!(
                "SELECT upper(name), lower(name), length(name), left(name, 3), right(name, 2), \
                 trim(descr), split_part(descr, ' ', 2), replace(name, 'a', 'A'), \
                 position(name IN name), concat(name, '|', kind), name || '#' || kind, \
                 substring(descr, 3, 5), starts_with(kind, 'h') \
                 FROM place WHERE _key = '{}'",
                row.key
            ),
            &[],
        )
        .unwrap()
    {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(rows.len(), 1);
    let v = &rows[0].values;
    assert_eq!(v[0], SqlValue::Text(row.name.to_uppercase()));
    assert_eq!(v[1], SqlValue::Text(row.name.to_lowercase()));
    assert_eq!(v[2], SqlValue::Int(row.name.chars().count() as i64));
    assert_eq!(
        v[3],
        SqlValue::Text(row.name.chars().take(3).collect::<String>())
    );
    assert_eq!(
        v[4],
        SqlValue::Text(row.name.chars().rev().take(2).collect::<String>().chars().rev().collect::<String>())
    );
    assert_eq!(v[5], SqlValue::Text(row.desc.trim().to_owned()));
    assert_eq!(
        v[6],
        SqlValue::Text(row.desc.split(' ').nth(1).unwrap_or_default().to_owned())
    );
    assert_eq!(v[7], SqlValue::Text(row.name.replace('a', "A")));
    assert_eq!(v[8], SqlValue::Int(1));
    assert_eq!(
        v[9],
        SqlValue::Text(format!("{}|{}", row.name, row.kind))
    );
    assert_eq!(
        v[10],
        SqlValue::Text(format!("{}#{}", row.name, row.kind))
    );
    assert_eq!(
        v[11],
        SqlValue::Text(row.desc.chars().skip(2).take(5).collect::<String>())
    );
    assert_eq!(v[12], SqlValue::Bool(row.kind.starts_with('h')));
}

/// The eighth law: a rewrite whose pre-image is a SET of ranges has no
/// atomic in this slice and is REFUSED, not answered by a scan.
#[test]
fn a_multi_range_rewrite_is_refused_and_a_missing_expression_index_too() {
    let (_dir, mut f) = open();
    timeline(&mut f.db);
    for sql in [
        "SELECT k FROM evt WHERE EXTRACT(MONTH FROM at) = 3",
        "SELECT k FROM evt WHERE EXTRACT(DAY FROM at) = 4",
        "SELECT k FROM evt WHERE EXTRACT(YEAR FROM at) <> 1950",
    ] {
        let error = f.db.sql(sql, &[]).unwrap_err();
        assert_eq!(
            error.reason(),
            Some(sekejap_lang::MULTI_RANGE_REASON),
            "{sql}"
        );
    }
    // No `lower(kind)` index exists on the FIXTURE's collection.
    let error = f
        .db
        .sql("SELECT _id FROM place WHERE lower(kind) = 'home'", &[])
        .unwrap_err();
    assert!(
        format!("{error}").contains("expression index"),
        "the refusal names the index that is missing: {error}"
    );
    // And an infix LIKE names the trigram family rather than taking a scan.
    let error = f
        .db
        .sql("SELECT _id FROM place WHERE kind LIKE '%om%'", &[])
        .unwrap_err();
    assert!(format!("{error}").contains("trigram"), "{error}");
}
