//! First combined-query slice. Expected result order and JSON-number equality
//! below are computed without scalar index codecs or the query executor.
use e4_prototype::{
    collections::{
        CandidateDriver, CollectionOptions, Database, EntityId, ProjectedValue, Projection,
        QueryBudget, QueryError, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue,
        SortDirection, WorkResource,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::ops::Bound;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn generous() -> QueryBudget {
    QueryBudget {
        candidates: 1_000_000,
        primary_reads: 1_000_000,
        scalar_postings: 1_000_000,
        graph_edges: 1_000_000,
        graph_visited: 1_000_000,
        spatial_postings: 1_000_000,
        text_postings: 1_000_000,
        text_tokens: 1_000_000,
        vector_locators: 1_000_000,
        vector_sidecars: 1_000_000,
        vector_lanes: 1_000_000,
        key_postings: 1_000_000,
        output_bytes: 16 << 20,
    }
}

#[test]
fn scalar_json_filters_keep_exact_numbers_null_missing_and_page_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "rows",
            vec![
                ("rank".into(), Kind::Int),
                ("real".into(), Kind::Real),
                ("nullable".into(), Kind::Int),
                ("profile".into(), Kind::Json),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let profile_integer = json!({
        "one":1,
        "nested":{"large":9007199254740993_u64,"maximum":u64::MAX},
        "array":[1,2,3]
    });
    let profile_float = json!({
        "array":[1.0,2.0,3.0],
        "nested":{"maximum":u64::MAX,"large":9007199254740993_u64},
        "one":1.0
    });
    let a = db
        .put(
            collection,
            "a",
            &json!({"rank":9007199254740992_i64,"real":1.0,"nullable":null,
                "profile":profile_integer}),
        )
        .unwrap();
    let b = db
        .put(
            collection,
            "b",
            &json!({"rank":9007199254740993_i64,"real":2.0,"profile":profile_float}),
        )
        .unwrap();
    let c = db
        .put(
            collection,
            "c",
            &json!({"rank":i64::MAX,"real":3.0,"nullable":7,"profile":{
                "one":1,"nested":{"large":9007199254740992_u64,"maximum":u64::MAX},
                "array":[1,2,3]}}),
        )
        .unwrap();
    db.commit().unwrap();
    let rank = db
        .create_scalar_index(collection, "rank", "rank", false)
        .unwrap();
    let nullable = db
        .create_scalar_index(collection, "nullable", "nullable", false)
        .unwrap();
    let real = db
        .create_scalar_index(collection, "real", "real", false)
        .unwrap();
    while !db.build_index_step(rank, 2).unwrap() {
        db.commit().unwrap();
    }
    while !db.build_index_step(nullable, 2).unwrap() {
        db.commit().unwrap();
    }
    while !db.build_index_step(real, 2).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();

    // Structural JSON equality compares numbers mathematically: integral 1.0
    // equals 1 at every nesting depth, while the exact large integers remain
    // distinct from their rounded binary64 neighbours.
    let expected_profile = json!({
        "nested":{"maximum":u64::MAX,"large":9007199254740993_u64},
        "array":[1.0,2,3.0],"one":1.0
    });
    let filters = [
        QueryFilter::Scalar {
            index: rank,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(9_007_199_254_740_992)),
                upper: Bound::Included(ScalarValue::I64(9_007_199_254_740_993)),
            },
        },
        QueryFilter::JsonEq {
            field: "profile",
            value: &expected_profile,
        },
    ];
    let projection = ["rank", "nullable", "profile"];
    let request = QueryRequest {
        collection,
        filters: &filters,
        order: QueryOrder::Scalar {
            index: rank,
            direction: SortDirection::Descending,
        },
        projection: Projection::Fields(&projection),
        total_limit: None,
        driver: CandidateDriver::Auto,
    };
    let mut query = db.prepare_query(request).unwrap();
    let first = query.next_page(1, generous(), || false).unwrap();
    assert_eq!(first.work.scalar_postings, 3); // two matches + upper-bound probe.
    assert_eq!(first.rows.iter().map(|row| row.id).collect::<Vec<_>>(), [b]);
    assert!(!first.done);
    assert_eq!(
        first.rows[0].projected[0].1,
        ProjectedValue::Value(json!(9_007_199_254_740_993_i64))
    );
    assert_eq!(first.rows[0].projected[1].1, ProjectedValue::Missing);
    let second = query.next_page(1, generous(), || false).unwrap();
    assert_eq!(
        second.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [a]
    );
    assert!(second.done);
    assert_eq!(second.rows[0].projected[1].1, ProjectedValue::Null);

    let null_filter = [QueryFilter::Scalar {
        index: nullable,
        predicate: ScalarFilter::IsNull,
    }];
    let missing_filter = [QueryFilter::Scalar {
        index: nullable,
        predicate: ScalarFilter::IsMissing,
    }];
    for (filters, expected) in [(&null_filter[..], a), (&missing_filter[..], b)] {
        let page = db
            .prepare_query(QueryRequest {
                collection,
                filters,
                order: QueryOrder::EntityId,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap()
            .next_page(4, generous(), || false)
            .unwrap();
        assert_eq!(
            page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [expected]
        );

        // Null and missing share one persisted scalar key, so a non-driving
        // predicate must still read the authoritative rows to distinguish
        // them. The rank index drives three rows plus its terminal probe, and
        // each of the three candidates is read once. The winner is NOT read a
        // fourth time: the walk already read that row, which is the existence
        // the re-fetch was asking about.
        let fallback = db
            .prepare_query(QueryRequest {
                collection,
                filters,
                order: QueryOrder::Scalar {
                    index: rank,
                    direction: SortDirection::Ascending,
                },
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Order,
            })
            .unwrap()
            .next_page(4, generous(), || false)
            .unwrap();
        assert_eq!(
            fallback.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [expected]
        );
        assert_eq!(fallback.work.scalar_postings, 4);
        assert_eq!(fallback.work.primary_reads, 3);
    }

    let rounded_max = json!({
        "one":1.0,"nested":{"large":9007199254740993_u64,
        "maximum":18446744073709551615.0_f64},"array":[1,2,3]
    });
    let rounded_filter = [QueryFilter::JsonEq {
        field: "profile",
        value: &rounded_max,
    }];
    let page = db
        .prepare_query(QueryRequest {
            collection,
            filters: &rounded_filter,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Entities,
        })
        .unwrap()
        .next_page(8, generous(), || false)
        .unwrap();
    assert!(page.rows.is_empty());
    assert!(page.done);

    let rounded_2p53 = json!({
        "one":1.0,"nested":{"large":9007199254740993.0_f64,"maximum":u64::MAX},
        "array":[1.0,2.0,3.0]
    });
    let rounded_2p53_filter = [QueryFilter::JsonEq {
        field: "profile",
        value: &rounded_2p53,
    }];
    let page = db
        .prepare_query(QueryRequest {
            collection,
            filters: &rounded_2p53_filter,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Entities,
        })
        .unwrap()
        .next_page(8, generous(), || false)
        .unwrap();
    assert_eq!(page.rows.iter().map(|row| row.id).collect::<Vec<_>>(), [c]);

    // Scalar predicates are declared-domain values; an f64 is not coerced to
    // this i64 index even when it has no fractional part.
    let wrong_kind = [QueryFilter::Scalar {
        index: rank,
        predicate: ScalarFilter::Eq(ScalarValue::F64(1.0)),
    }];
    assert!(db
        .prepare_query(QueryRequest {
            collection,
            filters: &wrong_kind,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .is_err());
    let integer_for_real = [QueryFilter::Scalar {
        index: real,
        predicate: ScalarFilter::Eq(ScalarValue::I64(i64::MAX)),
    }];
    assert!(db
        .prepare_query(QueryRequest {
            collection,
            filters: &integer_for_real,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .is_err());
    assert_ne!(c, a);
}

#[test]
fn scalar_driver_streams_and_pages_more_than_65536_matches_completely() {
    const ROWS: usize = 65_537;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "many",
            vec![("rank".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let rank = db
        .create_scalar_index(collection, "rank", "rank", false)
        .unwrap();
    assert!(db.build_index_step(rank, 1).unwrap());
    db.commit().unwrap();
    let mut inserted = Vec::with_capacity(ROWS);
    for position in 0..ROWS {
        inserted.push(
            db.put(
                collection,
                &format!("row/{position:05}"),
                &json!({"rank":i64::try_from(ROWS-position).unwrap()}),
            )
            .unwrap(),
        );
        if position % 512 == 511 {
            db.commit().unwrap();
            db.checkpoint().unwrap();
        }
    }
    db.commit().unwrap();

    let filters = [QueryFilter::Scalar {
        index: rank,
        predicate: ScalarFilter::Range {
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
        },
    }];
    let mut query = db
        .prepare_query(QueryRequest {
            collection,
            filters: &filters,
            order: QueryOrder::Scalar {
                index: rank,
                direction: SortDirection::Ascending,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let mut actual = Vec::with_capacity(ROWS);
    let mut postings = 0u64;
    let mut pages = 0u64;
    loop {
        let page = query.next_page(8192, generous(), || false).unwrap();
        postings += page.work.scalar_postings;
        pages += 1;
        actual.extend(page.rows.iter().map(|row| row.id));
        if page.done {
            break;
        }
    }
    inserted.reverse(); // rank was written in strictly descending ID order.
    assert_eq!(actual, inserted);
    assert_eq!(actual.len(), ROWS);
    // The whole posting range is streamed -- there is no 65,536 result cap --
    // but ONCE, not once per page: every page resumes at the previous page's
    // last key. This assertion read `>= ROWS` per page, which is the
    // re-walk-from-the-start cost it was written under.
    assert!(
        postings >= ROWS as u64 && postings <= ROWS as u64 + pages * 4,
        "{pages} pages streamed {postings} postings for {ROWS} rows; one pass \
         plus a resumed row per page is <= {}, one pass per page is {}",
        ROWS as u64 + pages * 4,
        ROWS as u64 * pages
    );
}

#[test]
fn snapshot_cursor_retries_after_cancel_and_budget_errors_then_reopens_current_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut writer = Database::create(&path, cfg()).unwrap();
    let collection = writer
        .create_collection(
            "rows",
            vec![("rank".into(), Kind::Int), ("profile".into(), Kind::Json)],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut old_ids = Vec::new();
    for rank in 0..12 {
        old_ids.push(
            writer
                .put(
                    collection,
                    &format!("row/{rank}"),
                    &json!({"rank":rank,"profile":{"generation":0}}),
                )
                .unwrap(),
        );
    }
    writer.commit().unwrap();
    let index = writer
        .create_scalar_index(collection, "rank", "rank", false)
        .unwrap();
    while !writer.build_index_step(index, 4).unwrap() {
        writer.commit().unwrap();
    }
    writer.commit().unwrap();
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();

    writer
        .update(
            collection,
            "row/0",
            &json!({"rank":100,"profile":{"generation":1}}),
        )
        .unwrap();
    writer.delete(collection, "row/1").unwrap();
    let new_id = writer
        .put(
            collection,
            "new",
            &json!({"rank":0,"profile":{"generation":1}}),
        )
        .unwrap();
    writer.commit().unwrap();

    let filters = [QueryFilter::Scalar {
        index,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(0)),
            upper: Bound::Included(ScalarValue::I64(11)),
        },
    }];
    let projection = ["profile"];
    let request = || QueryRequest {
        collection,
        filters: &filters,
        order: QueryOrder::Scalar {
            index,
            direction: SortDirection::Ascending,
        },
        projection: Projection::Fields(&projection),
        total_limit: None,
        driver: CandidateDriver::Auto,
    };
    let mut query = snapshot.prepare_query(request()).unwrap();
    let mut zero_probe = generous();
    zero_probe.scalar_postings = 0;
    assert!(matches!(
        query.next_page(5, zero_probe, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::ScalarPostings,
            attempted: 1,
            ..
        })
    ));
    let mut budget = generous();
    budget.candidates = 2;
    assert!(matches!(
        query.next_page(5, budget, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::Candidates,
            ..
        })
    ));
    let mut checks = 0;
    assert!(matches!(
        query.next_page(5, generous(), || {
            checks += 1;
            checks == 7
        }),
        Err(QueryError::Cancelled)
    ));
    let mut output_limited = generous();
    output_limited.output_bytes = 1;
    assert!(matches!(
        query.next_page(5, output_limited, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::OutputBytes,
            ..
        })
    ));

    let first = query.next_page(5, generous(), || false).unwrap();
    // 5 rows + the one lookahead that answers `done`. The posting range walks
    // in the order this query ranks by, so the page stops as soon as its heap
    // is full. It used to read all 12 rows plus a terminal probe to hand back
    // 5 -- the whole range, once per page.
    assert_eq!(first.work.scalar_postings, 6);
    assert_eq!(
        first.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        old_ids[..5]
    );
    assert_eq!(
        first.rows[0].projected[0].1,
        ProjectedValue::Value(json!({"generation":0}))
    );
    let second = query.next_page(7, generous(), || false).unwrap();
    assert_eq!(
        second.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        old_ids[5..]
    );
    assert!(second.done);

    drop(writer);
    let reopened = Database::open(&path, cfg()).unwrap();
    let current = reopened
        .prepare_query(request())
        .unwrap()
        .next_page(20, generous(), || false)
        .unwrap();
    let current_ids: Vec<EntityId> = current.rows.iter().map(|row| row.id).collect();
    assert_eq!(current_ids.first(), Some(&new_id));
    assert!(!current_ids.contains(&old_ids[0]));
    assert!(!current_ids.contains(&old_ids[1]));
    assert_eq!(current_ids.len(), 11);
}
