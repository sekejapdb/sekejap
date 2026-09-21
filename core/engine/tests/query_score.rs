//! Combined-score order: `QueryOrder::Score` against single-leaf orders and a
//! row-derived hybrid oracle. Source only — this file is not executed here.
use sekejap_core::{
    collections::{
        CandidateDriver, CollectionOptions, Database, EntityId, OrderValue, Projection,
        QueryBudget, QueryError, QueryFilter, QueryOrder, QueryRequest, QueryRow, ScoreExpr,
        SortDirection,
        TextMatch, VectorMetric,
    },
    spatial_math::Point,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::collections::BTreeMap;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn drain(
    db: &Database,
    request: QueryRequest<'_>,
    page_size: usize,
) -> Result<Vec<QueryRow>, QueryError> {
    let mut prepared = db.prepare_query(request)?;
    let mut rows = Vec::new();
    loop {
        let page = prepared.next_page(page_size, QueryBudget::unlimited(), || false)?;
        rows.extend(page.rows);
        if page.done {
            break;
        }
    }
    Ok(rows)
}

fn score_of(row: &QueryRow) -> f64 {
    match row.order {
        OrderValue::Score(value) | OrderValue::Bm25(value) | OrderValue::Distance(value) => value,
        ref other => panic!("expected a numeric order value, got {other:?}"),
    }
}

fn ids_of(rows: &[QueryRow]) -> Vec<EntityId> {
    rows.iter().map(|row| row.id).collect()
}

fn squared_l2(stored: [f32; 2], query: [f32; 2]) -> f64 {
    let mut sum = 0.0;
    for i in 0..2 {
        let delta = f64::from(stored[i]) - f64::from(query[i]);
        sum += delta * delta;
    }
    if sum == 0.0 {
        0.0
    } else {
        sum
    }
}

struct Corpus {
    db: Database,
    collection: sekejap_core::collections::CollectionId,
    body: sekejap_core::collections::IndexId,
    emb: sekejap_core::collections::IndexId,
    loc: sekejap_core::collections::IndexId,
    born: sekejap_core::collections::IndexId,
    embeddings: BTreeMap<EntityId, [f32; 2]>,
}

fn open_corpus(path: &std::path::Path, rows: usize) -> Corpus {
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "people",
            vec![
                ("body".into(), Kind::Text),
                ("embedding".into(), Kind::Vector(2)),
                ("loc".into(), Kind::Point),
                ("born".into(), Kind::Int),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut embeddings = BTreeMap::new();
    for i in 0..rows {
        let body = match i % 5 {
            0 => "flood river bank",
            1 => "flood road",
            2 => "river road",
            3 => "forest path",
            _ => "harbour quay",
        };
        let embedding = [(i % 10) as f32 / 10.0, (i % 7) as f32 / 7.0];
        let lon = (i % 170) as f64 * 0.01;
        let lat = (i % 80) as f64 * 0.01 - 20.0;
        let born = 1940 + (i % 86) as i64;
        let id = db
            .put(
                collection,
                &format!("p{i:03}"),
                &json!({
                    "body": body,
                    "embedding": embedding,
                    "loc": {"type":"Point","coordinates":[lon, lat]},
                    "born": born,
                }),
            )
            .unwrap();
        embeddings.insert(id, embedding);
    }
    db.commit().unwrap();
    let body = db.create_text_index(collection, "body", "body").unwrap();
    let emb = db
        .create_exact_vector_index(collection, "emb", "embedding")
        .unwrap();
    let loc = db.create_point_index(collection, "loc", "loc").unwrap();
    let born = db
        .create_scalar_index(collection, "born", "born", false)
        .unwrap();
    db.commit().unwrap();
    for index in [body, emb, loc, born] {
        db.build_index_to_ready(index, 256).unwrap();
    }
    db.commit().unwrap();
    Corpus {
        db,
        collection,
        body,
        emb,
        loc,
        born,
        embeddings,
    }
}

fn request<'a>(
    corpus: &'a Corpus,
    filters: &'a [QueryFilter<'a>],
    order: QueryOrder<'a>,
    total_limit: Option<usize>,
) -> QueryRequest<'a> {
    QueryRequest {
        collection: corpus.collection,
        filters,
        order,
        projection: Projection::Ids,
        total_limit,
        driver: CandidateDriver::Auto,
    }
}

/// (a) A single BM25 leaf equals `QueryOrder::Bm25` order and scores.
#[test]
fn score_bm25_leaf_equals_bm25_order() {
    let temp = tempfile::tempdir().unwrap();
    let corpus = open_corpus(&temp.path().join("db"), 500);
    let query = "flood";
    let bm25_rows = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::Bm25 {
                index: corpus.body,
                query,
                matching: TextMatch::Any,
            },
            None,
        ),
        64,
    )
    .unwrap();
    assert!(!bm25_rows.is_empty());
    let expr = ScoreExpr::Bm25 {
        index: corpus.body,
        query,
        matching: TextMatch::Any,
    };
    let score_rows = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Descending,
            },
            None,
        ),
        64,
    )
    .unwrap();
    assert_eq!(score_rows.len(), 500);
    let prefix = &score_rows[..bm25_rows.len()];
    assert_eq!(ids_of(prefix), ids_of(&bm25_rows));
    for (got, expected) in prefix.iter().zip(&bm25_rows) {
        assert_eq!(score_of(got).to_bits(), score_of(expected).to_bits());
    }
    for row in &score_rows[bm25_rows.len()..] {
        assert_eq!(score_of(row), 0.0);
    }
}

/// (b) A single VectorSimilarity leaf equals `QueryOrder::ExactVector` order.
#[test]
fn score_vector_leaf_equals_exact_vector_order() {
    let temp = tempfile::tempdir().unwrap();
    let corpus = open_corpus(&temp.path().join("db"), 500);
    let query = [0.5f32, 0.5];
    let exact = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::ExactVector {
                index: corpus.emb,
                query: &query,
                metric: VectorMetric::SquaredL2,
            },
            None,
        ),
        64,
    )
    .unwrap();
    let expr = ScoreExpr::VectorSimilarity {
        index: corpus.emb,
        query: &query,
        metric: VectorMetric::SquaredL2,
    };
    let scored = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Descending,
            },
            None,
        ),
        64,
    )
    .unwrap();
    assert_eq!(ids_of(&scored), ids_of(&exact));
    for (got, expected) in scored.iter().zip(&exact) {
        assert_eq!(score_of(got).to_bits(), (-score_of(expected)).to_bits());
    }
}

/// (c) A single Distance leaf equals `QueryOrder::Distance`.
#[test]
fn score_distance_leaf_equals_distance_order() {
    let temp = tempfile::tempdir().unwrap();
    let corpus = open_corpus(&temp.path().join("db"), 500);
    let center = Point::new(1.0, 0.0).unwrap();
    let distance = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::Distance {
                index: corpus.loc,
                center,
                direction: SortDirection::Ascending,
            },
            None,
        ),
        64,
    )
    .unwrap();
    let expr = ScoreExpr::Distance {
        index: corpus.loc,
        center,
    };
    let scored = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Ascending,
            },
            None,
        ),
        64,
    )
    .unwrap();
    assert_eq!(ids_of(&scored), ids_of(&distance));
    for (got, expected) in scored.iter().zip(&distance) {
        assert_eq!(score_of(got).to_bits(), score_of(expected).to_bits());
    }
}

/// (d) `0.5*Bm25 + 0.5*VectorSimilarity` matches a brute-force oracle from the
/// stored embeddings plus the engine BM25 map, ties by id, pages 1/3/7 resume.
#[test]
fn score_hybrid_matches_row_oracle_across_page_sizes() {
    let temp = tempfile::tempdir().unwrap();
    let corpus = open_corpus(&temp.path().join("db"), 500);
    let query_text = "flood";
    let query_vec = [0.4f32, 0.2];
    let bm25_rows = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::Bm25 {
                index: corpus.body,
                query: query_text,
                matching: TextMatch::Any,
            },
            None,
        ),
        64,
    )
    .unwrap();
    let mut bm25 = BTreeMap::new();
    for row in &bm25_rows {
        bm25.insert(row.id, score_of(row));
    }
    let mut oracle: Vec<(EntityId, f64)> = corpus
        .embeddings
        .iter()
        .map(|(&id, &stored)| {
            let bm25 = bm25.get(&id).copied().unwrap_or(0.0);
            let similarity = -squared_l2(stored, query_vec);
            (id, 0.5 * bm25 + 0.5 * similarity)
        })
        .collect();
    oracle.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let half = ScoreExpr::Lit(0.5);
    let bm25_leaf = ScoreExpr::Bm25 {
        index: corpus.body,
        query: query_text,
        matching: TextMatch::Any,
    };
    let vec_leaf = ScoreExpr::VectorSimilarity {
        index: corpus.emb,
        query: &query_vec,
        metric: VectorMetric::SquaredL2,
    };
    let text_term = ScoreExpr::Mul(&half, &bm25_leaf);
    let vec_term = ScoreExpr::Mul(&half, &vec_leaf);
    let expr = ScoreExpr::Add(&text_term, &vec_term);
    let order = QueryOrder::Score {
        expr: &expr,
        direction: SortDirection::Descending,
    };
    for page_size in [1, 3, 7] {
        let rows = drain(&corpus.db, request(&corpus, &[], order, None), page_size).unwrap();
        assert_eq!(rows.len(), oracle.len());
        for (got, (id, score)) in rows.iter().zip(&oracle) {
            assert_eq!(got.id, *id);
            assert_eq!(score_of(got).to_bits(), score.to_bits());
        }
    }
}

/// (e) A Scalar leaf over `born` plus a Lit.
#[test]
fn score_scalar_leaf_and_lit() {
    let temp = tempfile::tempdir().unwrap();
    let corpus = open_corpus(&temp.path().join("db"), 500);
    let ten = ScoreExpr::Lit(10.0);
    let born = ScoreExpr::Scalar { index: corpus.born };
    let expr = ScoreExpr::Add(&born, &ten);
    let rows = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Ascending,
            },
            None,
        ),
        64,
    )
    .unwrap();
    let scalar = drain(
        &corpus.db,
        request(
            &corpus,
            &[],
            QueryOrder::Scalar {
                index: corpus.born,
                direction: SortDirection::Ascending,
            },
            None,
        ),
        64,
    )
    .unwrap();
    assert_eq!(ids_of(&rows), ids_of(&scalar));
    let mut previous: Option<f64> = None;
    for row in &rows {
        let score = score_of(row);
        if let Some(previous) = previous {
            assert!(score >= previous);
        }
        previous = Some(score);
    }
    assert_eq!(rows.len(), 500);
}

fn prepare_score(
    db: &Database,
    collection: sekejap_core::collections::CollectionId,
    order: QueryOrder<'_>,
) -> Result<(), String> {
    db.prepare_query(QueryRequest {
        collection,
        filters: &[],
        order,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Auto,
    })
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn nest_neg<'a>(
    db: &'a Database,
    collection: sekejap_core::collections::CollectionId,
    inner: &'a ScoreExpr<'a>,
    remaining: usize,
    direction: SortDirection,
) -> Result<(), String> {
    if remaining == 0 {
        return prepare_score(
            db,
            collection,
            QueryOrder::Score {
                expr: inner,
                direction,
            },
        );
    }
    let neg = ScoreExpr::Neg(inner);
    nest_neg(db, collection, &neg, remaining - 1, direction)
}

fn nest_add_lits<'a>(
    db: &'a Database,
    collection: sekejap_core::collections::CollectionId,
    inner: &'a ScoreExpr<'a>,
    remaining: usize,
) -> Result<(), String> {
    if remaining == 0 {
        return prepare_score(
            db,
            collection,
            QueryOrder::Score {
                expr: inner,
                direction: SortDirection::Ascending,
            },
        );
    }
    let lit = ScoreExpr::Lit(1.0);
    let add = ScoreExpr::Add(inner, &lit);
    nest_add_lits(db, collection, &add, remaining - 1)
}

/// (f) Refusals: wrong family, other collection, depth > 32; ASC and DESC accepted.
#[test]
fn score_prepare_refusals_and_both_directions() {
    let temp = tempfile::tempdir().unwrap();
    let mut corpus = open_corpus(&temp.path().join("db"), 8);
    let other = corpus
        .db
        .create_collection(
            "other",
            vec![("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    corpus.db.put(other, "x", &json!({"born": 1})).unwrap();
    corpus.db.commit().unwrap();
    let foreign = corpus
        .db
        .create_scalar_index(other, "born", "born", false)
        .unwrap();
    corpus.db.build_index_to_ready(foreign, 256).unwrap();
    corpus.db.commit().unwrap();

    let wrong_family = ScoreExpr::Scalar { index: corpus.body };
    let msg = prepare_score(
        &corpus.db,
        corpus.collection,
        QueryOrder::Score {
            expr: &wrong_family,
            direction: SortDirection::Ascending,
        },
    )
    .unwrap_err();
    assert!(
        msg.contains("scalar") || msg.contains("family"),
        "wrong family: {msg}"
    );

    let foreign_leaf = ScoreExpr::Scalar { index: foreign };
    let msg = prepare_score(
        &corpus.db,
        corpus.collection,
        QueryOrder::Score {
            expr: &foreign_leaf,
            direction: SortDirection::Ascending,
        },
    )
    .unwrap_err();
    assert!(
        msg.contains("another collection"),
        "foreign scalar: {msg}"
    );

    let lit = ScoreExpr::Lit(1.0);
    let deep = nest_neg(
        &corpus.db,
        corpus.collection,
        &lit,
        32,
        SortDirection::Ascending,
    )
    .unwrap_err();
    assert!(deep.contains("depth"), "depth refusal: {deep}");

    nest_neg(
        &corpus.db,
        corpus.collection,
        &lit,
        31,
        SortDirection::Ascending,
    )
    .expect("depth 32 is accepted");

    let too_many = nest_add_lits(&corpus.db, corpus.collection, &lit, 8).unwrap_err();
    assert!(
        too_many.contains("leaf") || too_many.contains("8"),
        "leaf cap: {too_many}"
    );

    let expr = ScoreExpr::Lit(1.0);
    corpus
        .db
        .prepare_query(request(
            &corpus,
            &[],
            QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Ascending,
            },
            None,
        ))
        .unwrap();
    corpus
        .db
        .prepare_query(request(
            &corpus,
            &[],
            QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Descending,
            },
            None,
        ))
        .unwrap();
}

/// (g) NaN sorts last under both directions.
#[test]
fn score_nan_sorts_last_both_directions() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "nums",
            vec![("n".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut ids = Vec::new();
    for (key, n) in [("a", 2i64), ("b", 0), ("c", -3), ("d", 5)] {
        ids.push(db.put(collection, key, &json!({"n": n})).unwrap());
    }
    db.commit().unwrap();
    let index = db.create_scalar_index(collection, "n", "n", false).unwrap();
    db.build_index_to_ready(index, 256).unwrap();
    db.commit().unwrap();

    let scalar = ScoreExpr::Scalar { index };
    let one = ScoreExpr::Lit(1.0);
    let expr = ScoreExpr::Div(&one, &scalar);
    let mut asc = db
        .prepare_query(QueryRequest {
            collection,
            filters: &[],
            order: QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Ascending,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = asc
        .next_page(8, QueryBudget::unlimited(), || false)
        .unwrap();
    assert_eq!(page.rows.len(), 4);
    assert!(score_of(page.rows.last().unwrap()).is_nan());
    assert!(page.rows[..3].iter().all(|row| !score_of(row).is_nan()));

    let mut desc = db
        .prepare_query(QueryRequest {
            collection,
            filters: &[],
            order: QueryOrder::Score {
                expr: &expr,
                direction: SortDirection::Descending,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = desc
        .next_page(8, QueryBudget::unlimited(), || false)
        .unwrap();
    assert_eq!(page.rows.len(), 4);
    assert!(score_of(page.rows.last().unwrap()).is_nan());
    assert!(page.rows[..3].iter().all(|row| !score_of(row).is_nan()));
}


