//! Combined-query acceptance over the deterministic Phase 2 app fixture.
//! Expected memberships and ranking math below are derived directly from the
//! fixture rather than from family query helpers.
use e4_prototype::{
    collections::{
        ApproxVectorMethod, BfsRequest, CandidateDriver, CollectionId, CollectionOptions, Database,
        Direction, EntityId, Error, GraphContextId, IndexId, OrderValue, PointFilter,
        ProjectedValue, Projection, QueryBudget, QueryDriver, QueryError, QueryFilter, QueryOrder,
        QueryRequest, ScalarFilter, ScalarValue, TextMatch, VectorMetric, WorkResource,
    },
    pagewal::PageWalStore,
    spatial_math::{within_radius, Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
};

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn point(lon: f64, lat: f64) -> Value {
    json!({"type":"Point","coordinates":[lon,lat]})
}

fn ordered(number: u64) -> Vec<u8> {
    let bytes = number.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

fn primary_key(id: EntityId) -> Vec<u8> {
    let mut key = vec![0x40];
    key.extend(ordered(id.collection.0.into()));
    key.extend(ordered(id.sequence));
    key
}

fn scalar_bool_posting_key(index: IndexId, value: bool, id: EntityId) -> Vec<u8> {
    let mut key = vec![0x70];
    key.extend(ordered(index.0));
    key.extend([1, u8::from(value)]);
    key.extend(ordered(id.sequence));
    key
}

fn generous() -> QueryBudget {
    QueryBudget {
        candidates: 10_000,
        primary_reads: 10_000,
        scalar_postings: 10_000,
        graph_edges: 10_000,
        graph_visited: 10_000,
        spatial_postings: 10_000,
        text_postings: 10_000,
        text_tokens: 10_000,
        vector_locators: 10_000,
        vector_sidecars: 10_000,
        vector_lanes: 20_000,
        output_bytes: 1 << 20,
    }
}

#[derive(Clone, Copy)]
struct Indexes {
    active: IndexId,
    position: IndexId,
    text: IndexId,
    vector: IndexId,
    quantized: IndexId,
}

struct Fixture {
    db: Database,
    people: CollectionId,
    ids: BTreeMap<&'static str, EntityId>,
    indexes: Indexes,
    knows: e4_prototype::collections::EdgeTypeId,
}

fn finish_build(db: &mut Database, index: IndexId) {
    while !db.build_index_step(index, 2).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
}

fn create_fixture(path: &Path) -> Fixture {
    let mut db = Database::create(path, cfg()).unwrap();
    db.enable_graph().unwrap();
    let people = db
        .create_collection(
            "people",
            vec![
                ("age".into(), Kind::Int),
                ("active".into(), Kind::Bool),
                ("body".into(), Kind::Text),
                ("embedding".into(), Kind::Vector(2)),
                ("position".into(), Kind::Point),
                ("profile".into(), Kind::Json),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let organizations = db
        .create_collection(
            "organizations",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let o0 = db
        .put(organizations, "o0", &json!({"name":"Council"}))
        .unwrap();
    let o1 = db
        .put(organizations, "o1", &json!({"name":"Agency"}))
        .unwrap();
    let other_p0 = db
        .put(organizations, "p0", &json!({"name":"Names are scoped"}))
        .unwrap();
    let rows = [
        (
            "p0",
            20,
            true,
            Some("flood flood river"),
            [1.0, 0.0],
            (0.0, 0.0),
            o0,
        ),
        (
            "p1",
            30,
            true,
            Some("flood road"),
            [1.0, 1.0],
            (1.0, 0.0),
            o0,
        ),
        (
            "p2",
            30,
            false,
            Some("river road"),
            [0.0, 1.0],
            (0.0, 1.0),
            o0,
        ),
        ("p3", 40, true, Some("forest"), [-1.0, 0.0], (2.0, 2.0), o1),
        ("p4", 50, false, None, [0.0, -1.0], (3.0, 3.0), o1),
        ("p5", 60, true, Some(""), [1.0, 0.0], (0.0, 0.0), o1),
    ];
    let mut ids = BTreeMap::new();
    for (key, age, active, body, embedding, (lon, lat), organization) in rows {
        let mut value = json!({
            "age":age,
            "active":active,
            "embedding":embedding,
            "position":point(lon,lat),
            "profile":{"codes":[1,2,3],"nested":{"enabled":true}}
        });
        value["body"] = body.map_or(Value::Null, Value::from);
        let id = db.put(people, key, &value).unwrap();
        ids.insert(key, id);
        db.link(id, "member_of", organization, "", &json!({}))
            .unwrap();
    }
    assert_ne!(ids["p0"], other_p0);
    let edges = [
        ("p0", "p1"),
        ("p0", "p2"),
        ("p1", "p3"),
        ("p2", "p3"),
        ("p3", "p0"),
        ("p4", "p5"),
    ];
    let mut knows = None;
    for (source, destination) in edges {
        let edge = db
            .link(ids[source], "knows", ids[destination], "", &json!({}))
            .unwrap();
        knows = Some(edge.edge_type);
    }
    db.commit().unwrap();

    let indexes = Indexes {
        active: db
            .create_scalar_index(people, "active", "active", false)
            .unwrap(),
        position: db
            .create_point_index(people, "position", "position")
            .unwrap(),
        text: db.create_text_index(people, "body", "body").unwrap(),
        vector: db
            .create_exact_vector_index(people, "embedding", "embedding")
            .unwrap(),
        quantized: db
            .create_quantized_vector_index(people, "embedding_int8", "embedding")
            .unwrap(),
    };
    db.commit().unwrap();
    for index in [
        indexes.active,
        indexes.position,
        indexes.text,
        indexes.vector,
        indexes.quantized,
    ] {
        finish_build(&mut db, index);
    }
    Fixture {
        db,
        people,
        ids,
        indexes,
        knows: knows.unwrap(),
    }
}

fn graph_request(f: &Fixture) -> BfsRequest {
    BfsRequest {
        seed: f.ids["p0"],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(f.knows),
        min_depth: 1,
        max_depth: 2,
        include_seed: false,
        max_visited: 32,
        max_edges: 64,
        result_limit: 32,
    }
}

fn cosine(distance: &OrderValue) -> f64 {
    match distance {
        OrderValue::Distance(value) => *value,
        value => panic!("expected vector distance, got {value:?}"),
    }
}

fn independent_cosine(stored: [f64; 2], query: [f32; 2]) -> Option<f64> {
    let query = [f64::from(query[0]), f64::from(query[1])];
    let dot = stored[0] * query[0] + stored[1] * query[1];
    let stored_norm = stored[0] * stored[0] + stored[1] * stored[1];
    let query_norm = query[0] * query[0] + query[1] * query[1];
    if stored_norm == 0.0 {
        return None;
    }
    let distance = 1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt());
    Some(if distance == 0.0 { 0.0 } else { distance })
}

fn independent_quantized_oracle(
    rows: &[(EntityId, [f32; 2])],
    query: [f32; 2],
    ef: usize,
) -> Vec<(EntityId, f64)> {
    let mut approximate = Vec::new();
    for &(id, vector) in rows {
        let maximum = vector
            .iter()
            .map(|lane| f64::from(*lane).abs())
            .fold(0.0f64, f64::max);
        let scale = maximum / 127.0;
        let reconstructed = vector.map(|lane| {
            let code = if scale == 0.0 {
                0
            } else {
                (f64::from(lane) / scale).round().clamp(-127.0, 127.0) as i8
            };
            f64::from(code) * scale
        });
        if let Some(distance) = independent_cosine(reconstructed, query) {
            approximate.push((id, distance));
        }
    }
    approximate.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    approximate.truncate(ef);
    let mut exact = approximate
        .into_iter()
        .filter_map(|(id, _)| {
            let vector = rows.iter().find(|row| row.0 == id).unwrap().1;
            independent_cosine(vector.map(f64::from), query).map(|distance| (id, distance))
        })
        .collect::<Vec<_>>();
    exact.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    exact
}

fn point_page(
    db: &Database,
    people: CollectionId,
    index: IndexId,
    predicate: PointFilter,
    driver: CandidateDriver,
) -> e4_prototype::collections::QueryPage {
    let filters = [QueryFilter::Point { index, predicate }];
    let mut query = db
        .prepare_query(QueryRequest {
            collection: people,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver,
        })
        .unwrap();
    query.next_page(32, generous(), || false).unwrap()
}

fn text_page(
    db: &Database,
    people: CollectionId,
    index: IndexId,
    text: &str,
    matching: TextMatch,
    driver: CandidateDriver,
) -> e4_prototype::collections::QueryPage {
    let filters = [QueryFilter::Text {
        index,
        query: text,
        matching,
    }];
    let mut query = db
        .prepare_query(QueryRequest {
            collection: people,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver,
        })
        .unwrap();
    query.next_page(32, generous(), || false).unwrap()
}

#[test]
fn graph_scalar_spatial_text_and_vector_apply_before_ranked_top_k() {
    let temp = tempfile::tempdir().unwrap();
    let mut f = create_fixture(&temp.path().join("db"));
    let profile = json!({"codes":[1.0,2,3.0],"nested":{"enabled":true}});
    let filters = [
        QueryFilter::Graph(graph_request(&f)),
        QueryFilter::Scalar {
            index: f.indexes.active,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Point {
            index: f.indexes.position,
            predicate: PointFilter::Bbox(Bounds::new(0.0, 1.0, 0.0, 1.0).unwrap()),
        },
        QueryFilter::JsonEq {
            field: "profile",
            value: &profile,
        },
    ];
    let projection = ["embedding", "profile"];
    let mut query =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &filters,
            order: QueryOrder::ExactVector {
                index: f.indexes.vector,
                query: &[1.0, 0.0],
                metric: VectorMetric::Cosine,
            },
            projection: Projection::Fields(&projection),
            total_limit: Some(2),
            driver: CandidateDriver::Auto,
        })
        .unwrap();

    let mut no_primary = generous();
    no_primary.primary_reads = 0;
    assert!(matches!(
        query.next_page(2, no_primary, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::PrimaryReads,
            attempted: 1,
            ..
        })
    ));
    let mut no_edges = generous();
    no_edges.graph_edges = 0;
    assert!(matches!(
        query.next_page(2, no_edges, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::GraphEdges,
            attempted: 1,
            ..
        })
    ));
    let page = query.next_page(2, generous(), || false).unwrap();
    assert_eq!(page.driver, QueryDriver::Graph { filter: 0 });
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![f.ids["p1"]]
    );
    assert!((cosine(&page.rows[0].order) - (1.0 - 1.0 / 2.0_f64.sqrt())).abs() <= 1e-15);
    assert_eq!(
        page.rows[0].projected[0].1,
        ProjectedValue::Value(json!([1.0, 1.0]))
    );
    assert!(page.work.graph_edges > 0);
    assert!(page.work.graph_visited > 0);
    assert!(page.work.spatial_postings > 0);
    assert!(page.work.vector_sidecars > 0);
    assert!(page.work.vector_lanes > 0);

    // Text is a constraint before vector top-k. p5 is globally tied for best
    // cosine distance but cannot enter because its empty body does not match.
    let text_filters = [
        QueryFilter::Scalar {
            index: f.indexes.active,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Text {
            index: f.indexes.text,
            query: "flood",
            matching: TextMatch::Any,
        },
    ];
    let mut query =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &text_filters,
            order: QueryOrder::ExactVector {
                index: f.indexes.vector,
                query: &[1.0, 0.0],
                metric: VectorMetric::Cosine,
            },
            projection: Projection::Ids,
            total_limit: Some(2),
            driver: CandidateDriver::Order,
        })
        .unwrap();
    let page = query.next_page(2, generous(), || false).unwrap();
    assert_eq!(page.driver, QueryDriver::ExactVector(f.indexes.vector));
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![f.ids["p0"], f.ids["p1"]]
    );
    assert!(page.work.text_postings > 0);

    // BM25 oracle: N=5, total length=8, df(flood)=2, K1=1.2, B=.75.
    let mut bm25 =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &[],
            order: QueryOrder::Bm25 {
                index: f.indexes.text,
                query: "flood",
                matching: TextMatch::Any,
            },
            projection: Projection::Ids,
            total_limit: Some(2),
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = bm25.next_page(2, generous(), || false).unwrap();
    assert_eq!(page.driver, QueryDriver::Text(f.indexes.text));
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![f.ids["p0"], f.ids["p1"]]
    );
    let idf = (1.0_f64 + (5.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
    let expected = [
        idf * (2.0 * 2.2) / (2.0 + 1.2 * (0.25 + 0.75 * 3.0 / 1.6)),
        idf * 2.2 / (1.0 + 1.2 * (0.25 + 0.75 * 2.0 / 1.6)),
    ];
    for (row, expected) in page.rows.iter().zip(expected) {
        let OrderValue::Bm25(actual) = row.order else {
            panic!("BM25 order value")
        };
        assert!((actual - expected).abs() <= 1e-14);
    }

    let all_terms = [QueryFilter::Text {
        index: f.indexes.text,
        query: "river flood",
        matching: TextMatch::All,
    }];
    let mut query =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &all_terms,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = query.next_page(8, generous(), || false).unwrap();
    assert_eq!(page.driver, QueryDriver::Text(f.indexes.text));
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![f.ids["p0"]]
    );
    assert_eq!(
        text_page(
            &f.db,
            f.people,
            f.indexes.text,
            "river flood",
            TextMatch::All,
            CandidateDriver::Entities,
        )
        .rows,
        text_page(
            &f.db,
            f.people,
            f.indexes.text,
            "river flood",
            TextMatch::All,
            CandidateDriver::Filter(0),
        )
        .rows
    );

    let text_entities = text_page(
        &f.db,
        f.people,
        f.indexes.text,
        "flood river",
        TextMatch::Any,
        CandidateDriver::Entities,
    );
    let text_index = text_page(
        &f.db,
        f.people,
        f.indexes.text,
        "flood river",
        TextMatch::Any,
        CandidateDriver::Filter(0),
    );
    assert_eq!(text_entities.rows, text_index.rows);
    assert_eq!(text_index.driver, QueryDriver::Text(f.indexes.text));
    assert!(text_index.work.primary_reads < text_entities.work.primary_reads);
    let retry_filters = [QueryFilter::Text {
        index: f.indexes.text,
        query: "flood",
        matching: TextMatch::Any,
    }];
    let mut retry =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &retry_filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let mut checks = 0;
    assert!(matches!(
        retry.next_page(8, generous(), || {
            checks += 1;
            checks == 4
        }),
        Err(QueryError::Cancelled)
    ));
    let mut no_postings = generous();
    no_postings.text_postings = 0;
    assert!(matches!(
        retry.next_page(8, no_postings, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::TextPostings,
            attempted: 1,
            ..
        })
    ));
    assert_eq!(
        retry
            .next_page(8, generous(), || false)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![f.ids["p0"], f.ids["p1"]]
    );

    let radius = [QueryFilter::Point {
        index: f.indexes.position,
        predicate: PointFilter::Radius {
            center: Point::new(0.0, 0.0).unwrap(),
            radius_metres: 0.0,
        },
    }];
    let mut query =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &radius,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = query.next_page(8, generous(), || false).unwrap();
    assert_eq!(
        page.driver,
        QueryDriver::Spatial {
            index: f.indexes.position,
            fallback_world: false,
        }
    );
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![f.ids["p0"], f.ids["p5"]]
    );
    let zero_radius = PointFilter::Radius {
        center: Point::new(0.0, 0.0).unwrap(),
        radius_metres: 0.0,
    };
    assert_eq!(
        point_page(
            &f.db,
            f.people,
            f.indexes.position,
            zero_radius,
            CandidateDriver::Entities,
        )
        .rows,
        point_page(
            &f.db,
            f.people,
            f.indexes.position,
            zero_radius,
            CandidateDriver::Filter(0),
        )
        .rows
    );

    let bounds = Bounds::new(0.0, 1.0, 0.0, 1.0).unwrap();
    let point_entities = point_page(
        &f.db,
        f.people,
        f.indexes.position,
        PointFilter::Bbox(bounds),
        CandidateDriver::Entities,
    );
    let point_index = point_page(
        &f.db,
        f.people,
        f.indexes.position,
        PointFilter::Bbox(bounds),
        CandidateDriver::Filter(0),
    );
    assert_eq!(point_entities.rows, point_index.rows);
    assert_eq!(
        point_index
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![f.ids["p0"], f.ids["p1"], f.ids["p2"], f.ids["p5"]]
    );
    assert_eq!(
        point_index.driver,
        QueryDriver::Spatial {
            index: f.indexes.position,
            fallback_world: false,
        }
    );
    assert!(point_index.work.primary_reads < point_entities.work.primary_reads);
    let point_filters = [QueryFilter::Point {
        index: f.indexes.position,
        predicate: PointFilter::Bbox(bounds),
    }];
    let mut retry =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &point_filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let mut no_postings = generous();
    no_postings.spatial_postings = 0;
    assert!(matches!(
        retry.next_page(8, no_postings, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::SpatialPostings,
            attempted: 1,
            ..
        })
    ));
    assert_eq!(
        retry.next_page(8, generous(), || false).unwrap().rows,
        point_index.rows
    );

    let east =
        f.db.put(f.people, "east", &json!({"position":point(179.5,5.0)}))
            .unwrap();
    let west =
        f.db.put(f.people, "west", &json!({"position":point(-179.5,-5.0)}))
            .unwrap();
    f.db.put(
        f.people,
        "not-dateline",
        &json!({"position":point(0.0,0.0)}),
    )
    .unwrap();
    f.db.commit().unwrap();
    let dateline = PointFilter::Bbox(Bounds::new(170.0, -170.0, -10.0, 10.0).unwrap());
    let entities = point_page(
        &f.db,
        f.people,
        f.indexes.position,
        dateline,
        CandidateDriver::Entities,
    );
    let indexed = point_page(
        &f.db,
        f.people,
        f.indexes.position,
        dateline,
        CandidateDriver::Filter(0),
    );
    assert_eq!(entities.rows, indexed.rows);
    assert_eq!(
        indexed.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![east, west]
    );

    let world = point_page(
        &f.db,
        f.people,
        f.indexes.position,
        PointFilter::Bbox(Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap()),
        CandidateDriver::Filter(0),
    );
    assert_eq!(
        world.driver,
        QueryDriver::Spatial {
            index: f.indexes.position,
            fallback_world: true,
        }
    );

    // A failed mixed write is explicitly rolled back and cannot leak through
    // any maintained family.
    let exact_before = mixed_ids(&f.db, f.people, &f.ids, f.indexes, f.knows, false);
    let approximate_before = mixed_ids(&f.db, f.people, &f.ids, f.indexes, f.knows, true);
    f.db.update(
        f.people,
        "p1",
        &json!({"age":35,"body":"forest","position":point(5.0,5.0)}),
    )
    .unwrap();
    f.db.unlink(f.ids["p0"], "knows", f.ids["p1"], "").unwrap();
    assert!(f
        .db
        .update(f.people, "p3", &json!({"embedding":[1.0]}))
        .is_err());
    f.db.rollback().unwrap();
    assert_eq!(
        f.db.get(f.people, "p1").unwrap().unwrap().document["age"],
        json!(30)
    );
    assert_eq!(
        mixed_ids(&f.db, f.people, &f.ids, f.indexes, f.knows, false),
        exact_before
    );
    assert_eq!(
        mixed_ids(&f.db, f.people, &f.ids, f.indexes, f.knows, true),
        approximate_before
    );
}

#[test]
fn phrase_refines_all_term_candidates_before_rank_and_across_drivers() {
    let temp = tempfile::tempdir().unwrap();
    let mut f = create_fixture(&temp.path().join("db"));
    let common = |body: &str, embedding: [f64; 2]| {
        json!({
            "age":25,
            "active":true,
            "body":body,
            "embedding":embedding,
            "position":point(0.5,0.5),
            "profile":{"codes":[1,2,3],"nested":{"enabled":true}}
        })
    };
    let reversed =
        f.db.put(
            f.people,
            "phrase-reversed",
            &common("river flood", [0.9, 0.1]),
        )
        .unwrap();
    let exact =
        f.db.put(
            f.people,
            "phrase-exact",
            &common("flood river", [-1.0, 0.0]),
        )
        .unwrap();
    f.db.commit().unwrap();

    // The closest vector has both terms in reverse order. Exact phrase
    // refinement must happen before the one-row vector top-k.
    let phrase = [QueryFilter::Text {
        index: f.indexes.text,
        query: "flood river",
        matching: TextMatch::Phrase,
    }];
    let mut vector =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &phrase,
            order: QueryOrder::ExactVector {
                index: f.indexes.vector,
                query: &[0.9, 0.1],
                metric: VectorMetric::Cosine,
            },
            projection: Projection::Ids,
            total_limit: Some(1),
            driver: CandidateDriver::Order,
        })
        .unwrap();
    let page = vector.next_page(1, generous(), || false).unwrap();
    assert_eq!(page.rows[0].id, f.ids["p0"]);
    assert_ne!(page.rows[0].id, reversed);
    assert!(page.work.text_tokens > 0);

    // BM25 still sums distinct terms, but only the authoritative ordered
    // phrase is eligible for its top-k.
    let mut bm25 =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &[],
            order: QueryOrder::Bm25 {
                index: f.indexes.text,
                query: "river flood",
                matching: TextMatch::Phrase,
            },
            projection: Projection::Ids,
            total_limit: Some(1),
            driver: CandidateDriver::Order,
        })
        .unwrap();
    let page = bm25.next_page(1, generous(), || false).unwrap();
    assert_eq!(page.rows[0].id, reversed);
    assert!(matches!(page.rows[0].order, OrderValue::Bm25(_)));

    let text = text_page(
        &f.db,
        f.people,
        f.indexes.text,
        "flood river",
        TextMatch::Phrase,
        CandidateDriver::Filter(0),
    );
    let entities = text_page(
        &f.db,
        f.people,
        f.indexes.text,
        "flood river",
        TextMatch::Phrase,
        CandidateDriver::Entities,
    );
    assert_eq!(text.rows, entities.rows);
    assert_eq!(
        text.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![f.ids["p0"], exact]
    );
    assert!(text.work.text_tokens > 0);

    let scalar_and_phrase = [
        QueryFilter::Scalar {
            index: f.indexes.active,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Text {
            index: f.indexes.text,
            query: "flood river",
            matching: TextMatch::Phrase,
        },
    ];
    let mut scalar =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &scalar_and_phrase,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let scalar = scalar.next_page(32, generous(), || false).unwrap();
    assert_eq!(scalar.rows, text.rows);
    assert_eq!(scalar.driver, QueryDriver::Scalar(f.indexes.active));

    let mut retry =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &phrase,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let mut no_tokens = generous();
    no_tokens.text_tokens = 0;
    assert!(matches!(
        retry.next_page(32, no_tokens, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::TextTokens,
            attempted: 1,
            ..
        })
    ));
    let mut calls = 0;
    assert!(matches!(
        retry.next_page(32, generous(), || {
            calls += 1;
            calls == 10
        }),
        Err(QueryError::Cancelled)
    ));
    assert_eq!(
        retry.next_page(32, generous(), || false).unwrap().rows,
        text.rows
    );
}

fn ascii_terms(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn contains_ordered_phrase(text: &str, phrase: &str) -> bool {
    let tokens = ascii_terms(text);
    let needle = ascii_terms(phrase);
    !needle.is_empty() && tokens.windows(needle.len()).any(|window| window == needle)
}

fn outgoing_bfs(
    edges: &[(EntityId, EntityId)],
    seed: EntityId,
    min_depth: usize,
    max_depth: usize,
) -> BTreeSet<EntityId> {
    let mut adj = BTreeMap::<EntityId, Vec<EntityId>>::new();
    for &(source, destination) in edges {
        adj.entry(source).or_default().push(destination);
    }
    let mut seen = BTreeSet::from([seed]);
    let mut members = BTreeSet::new();
    let mut queue = VecDeque::from([(seed, 0usize)]);
    while let Some((node, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        for &next in adj.get(&node).into_iter().flatten() {
            if !seen.insert(next) {
                continue;
            }
            let next_depth = depth + 1;
            if next_depth >= min_depth {
                members.insert(next);
            }
            queue.push_back((next, next_depth));
        }
    }
    members
}

#[test]
fn phrase_graph_and_spatial_intersect_before_top_k() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut f = create_fixture(&path);
    let phrase = "alpha beta";
    let center = Point::new(0.0, 0.0).unwrap();
    let radius_metres = 5_000.0;
    let person = |body: &str, lon: f64, lat: f64| {
        json!({
            "age":25,
            "active":true,
            "body":body,
            "embedding":[0.0, 0.0],
            "position":point(lon, lat),
            "profile":{"codes":[1,2,3],"nested":{"enabled":true}}
        })
    };
    let mut rows = BTreeMap::new();
    let fixture_bodies = [
        ("p0", Some("flood flood river"), 0.0, 0.0),
        ("p1", Some("flood road"), 1.0, 0.0),
        ("p2", Some("river road"), 0.0, 1.0),
        ("p3", Some("forest"), 2.0, 2.0),
        ("p4", None, 3.0, 3.0),
        ("p5", Some(""), 0.0, 0.0),
    ];
    for (key, body, lon, lat) in fixture_bodies {
        rows.insert(f.ids[key], (body.map(str::to_owned), lon, lat));
    }
    let mut put = |key: &str, body: &str, lon: f64, lat: f64| {
        let id = f.db.put(f.people, key, &person(body, lon, lat)).unwrap();
        rows.insert(id, (Some(body.to_owned()), lon, lat));
        id
    };
    let in_all = put("in-all", "alpha beta gamma", 0.0, 0.0);
    let hop = put("hop", "unrelated", 0.0, 0.0);
    let depth2 = put("depth2-match", "start alpha beta end", 0.005, 0.0);
    let depth2_far = put("depth2-far", "alpha beta far", 20.0, 0.0);
    let depth3 = put("depth3-match", "alpha beta late", 0.0, 0.0);
    let reversed = put("reversed-near", "beta alpha", 0.0, 0.0);
    let gapped = put("gapped-near", "alpha xxx beta", 0.0, 0.0);
    let _orphan = put("orphan-match", "alpha beta orphan", 0.0, 0.0);
    drop(put);
    let mut edges = vec![
        (f.ids["p0"], f.ids["p1"]),
        (f.ids["p0"], f.ids["p2"]),
        (f.ids["p1"], f.ids["p3"]),
        (f.ids["p2"], f.ids["p3"]),
        (f.ids["p3"], f.ids["p0"]),
        (f.ids["p4"], f.ids["p5"]),
    ];
    for (source, destination) in [
        (f.ids["p0"], in_all),
        (f.ids["p0"], hop),
        (f.ids["p0"], reversed),
        (f.ids["p0"], gapped),
        (hop, depth2),
        (hop, depth2_far),
        (depth2, depth3),
    ] {
        f.db.link(source, "knows", destination, "", &json!({}))
            .unwrap();
        edges.push((source, destination));
    }
    f.db.commit().unwrap();

    let graph = outgoing_bfs(&edges, f.ids["p0"], 1, 2);
    let expected: Vec<_> = rows
        .iter()
        .filter(|(id, (body, lon, lat))| {
            graph.contains(*id)
                && body
                    .as_deref()
                    .is_some_and(|text| contains_ordered_phrase(text, phrase))
                && within_radius(center, Point::new(*lon, *lat).unwrap(), radius_metres).unwrap()
        })
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(expected, vec![in_all, depth2]);

    let filters = [
        QueryFilter::Graph(graph_request(&f)),
        QueryFilter::Point {
            index: f.indexes.position,
            predicate: PointFilter::Radius {
                center,
                radius_metres,
            },
        },
        QueryFilter::Text {
            index: f.indexes.text,
            query: phrase,
            matching: TextMatch::Phrase,
        },
    ];
    let people = f.people;
    let request = || QueryRequest {
        collection: people,
        filters: &filters,
        order: QueryOrder::EntityId,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Auto,
    };
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
    let page = snapshot
        .prepare_query(request())
        .unwrap()
        .next_page(32, generous(), || false)
        .unwrap();
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        expected
    );
    assert!(page.work.graph_edges > 0);
    assert!(page.work.spatial_postings > 0);
    assert!(page.work.text_tokens > 0);

    f.db.unlink(f.ids["p0"], "knows", in_all, "").unwrap();
    f.db.update(f.people, "depth2-match", &person("gone", 0.005, 0.0))
        .unwrap();
    f.db.commit().unwrap();
    let still = snapshot
        .prepare_query(request())
        .unwrap()
        .next_page(32, generous(), || false)
        .unwrap();
    assert_eq!(
        still.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        expected
    );
    let live =
        f.db.prepare_query(request())
            .unwrap()
            .next_page(32, generous(), || false)
            .unwrap();
    assert!(live.rows.is_empty());
}

#[test]
fn phrase_pages_are_disjoint_complete_stable_and_snapshot_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    let phrase = "steady rain";
    let mut rows = Vec::new();
    for n in 0..7 {
        let body = format!("keep {phrase} {n}");
        let id = db
            .put(collection, &format!("hit/{n}"), &json!({"body": body}))
            .unwrap();
        rows.push((id, body));
    }
    for (key, body) in [
        ("reversed", "rain steady"),
        ("gapped", "steady cold rain"),
        ("other", "dry road"),
    ] {
        let id = db.put(collection, key, &json!({"body": body})).unwrap();
        rows.push((id, body.to_owned()));
    }
    db.commit().unwrap();
    let index = db.create_text_index(collection, "body", "body").unwrap();
    finish_build(&mut db, index);
    let expected: Vec<_> = rows
        .iter()
        .filter(|(_, body)| contains_ordered_phrase(body, phrase))
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(expected.len(), 7);

    let filters = [QueryFilter::Text {
        index,
        query: phrase,
        matching: TextMatch::Phrase,
    }];
    let request = || QueryRequest {
        collection,
        filters: &filters,
        order: QueryOrder::EntityId,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Filter(0),
    };
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
    let mut query = snapshot.prepare_query(request()).unwrap();

    let first = query.next_page(2, generous(), || false).unwrap();
    assert_eq!(first.rows.len(), 2);
    assert!(!first.done);

    db.update(collection, "hit/2", &json!({"body": "steady gone"}))
        .unwrap();
    db.put(
        collection,
        "hit/new",
        &json!({"body": format!("fresh {phrase}")}),
    )
    .unwrap();
    db.commit().unwrap();

    let mut pages = vec![first.rows.iter().map(|row| row.id).collect::<Vec<_>>()];
    loop {
        let page = query.next_page(2, generous(), || false).unwrap();
        pages.push(page.rows.iter().map(|row| row.id).collect());
        if page.done {
            break;
        }
    }
    assert!(
        pages.len() > 2,
        "expected more than two pages, got {}",
        pages.len()
    );
    let mut union = Vec::new();
    let mut seen = BTreeSet::new();
    for page in &pages {
        assert!(!page.is_empty());
        for id in page {
            assert!(seen.insert(*id), "page id {id:?} repeated across pages");
            union.push(*id);
        }
    }
    assert_eq!(union, expected);
    assert_eq!(seen, expected.iter().copied().collect::<BTreeSet<_>>());

    let live: Vec<_> = db
        .prepare_query(request())
        .unwrap()
        .next_page(32, generous(), || false)
        .unwrap()
        .rows
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_ne!(live, expected);
    assert!(!live.contains(&rows[2].0));
}

#[test]
fn text_driver_uses_strict_scalar_membership_with_retry_pages_and_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut f = create_fixture(&path);
    let people = f.people;
    let indexes = f.indexes;
    let filters = [
        QueryFilter::Scalar {
            index: indexes.active,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Text {
            index: indexes.text,
            query: "flood",
            matching: TextMatch::Any,
        },
    ];
    let request = || QueryRequest {
        collection: people,
        filters: &filters,
        order: QueryOrder::EntityId,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Filter(1),
    };
    let mut query = f.db.prepare_query(request()).unwrap();

    let mut no_scalar_probe = generous();
    no_scalar_probe.scalar_postings = 0;
    assert!(matches!(
        query.next_page(1, no_scalar_probe, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::ScalarPostings,
            attempted: 1,
            ..
        })
    ));
    let mut cancellation_checks = 0;
    assert!(matches!(
        query.next_page(1, generous(), || {
            cancellation_checks += 1;
            cancellation_checks == 6
        }),
        Err(QueryError::Cancelled)
    ));

    let first = query.next_page(1, generous(), || false).unwrap();
    assert_eq!(first.driver, QueryDriver::Text(indexes.text));
    assert_eq!(first.rows[0].id, f.ids["p0"]);
    assert_eq!(first.work.scalar_postings, 2);
    assert_eq!(first.work.primary_reads, 1); // winner existence only
    assert!(!first.done);
    let second = query.next_page(1, generous(), || false).unwrap();
    assert_eq!(second.rows[0].id, f.ids["p1"]);
    assert_eq!(second.work.scalar_postings, 2);
    assert_eq!(second.work.primary_reads, 1);
    assert!(second.done);

    // Entity candidates already carry their primary row. Reuse it for the
    // equality predicate instead of spending a scalar-posting probe.
    let mut entity_query =
        f.db.prepare_query(QueryRequest {
            collection: people,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Entities,
        })
        .unwrap();
    let mut no_scalar_probe = generous();
    no_scalar_probe.scalar_postings = 0;
    let entity_page = entity_query
        .next_page(8, no_scalar_probe, || false)
        .unwrap();
    assert_eq!(
        entity_page
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![f.ids["p0"], f.ids["p1"]]
    );
    assert_eq!(entity_page.work.scalar_postings, 0);

    let old = Database::open_snapshot(&path, cfg()).unwrap();
    let mut changed = f.db.get(f.people, "p1").unwrap().unwrap().document;
    changed["active"] = json!(false);
    f.db.update(f.people, "p1", &changed).unwrap();
    f.db.commit().unwrap();

    let read_ids = |db: &Database| {
        db.prepare_query(request())
            .unwrap()
            .next_page(8, generous(), || false)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(read_ids(&old), vec![f.ids["p0"], f.ids["p1"]]);
    assert_eq!(read_ids(&f.db), vec![f.ids["p0"]]);
}

#[test]
fn text_driven_scalar_membership_rejects_nonempty_posting_values() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let Fixture {
        db,
        people,
        ids,
        indexes,
        ..
    } = create_fixture(&path);
    // The posting is damaged where it actually lives: a version-2 scalar index
    // keeps its entries in its own tree, and the key is byte-identical there.
    let tree = db.index_tree(indexes.active).unwrap();
    drop(db);
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    let key = scalar_bool_posting_key(indexes.active, true, ids["p0"]);
    match tree {
        None => raw.put(&key, &[1]).unwrap(),
        Some((id, root)) => {
            assert_eq!(raw.tree_put(id, root, &key, &[1]).unwrap(), root);
        }
    }
    raw.commit().unwrap();
    drop(raw);

    let db = Database::open_snapshot(&path, cfg()).unwrap();
    let filters = [
        QueryFilter::Scalar {
            index: indexes.active,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Text {
            index: indexes.text,
            query: "flood",
            matching: TextMatch::Any,
        },
    ];
    let result = db
        .prepare_query(QueryRequest {
            collection: people,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(1),
        })
        .unwrap()
        .next_page(8, generous(), || false);
    assert!(matches!(
        result,
        Err(QueryError::Database(Error::Corrupt(_)))
    ));
}

fn mixed_ids(
    db: &Database,
    people: CollectionId,
    ids: &BTreeMap<&str, EntityId>,
    indexes: Indexes,
    knows: e4_prototype::collections::EdgeTypeId,
    approximate: bool,
) -> Vec<EntityId> {
    let graph = BfsRequest {
        seed: ids["p0"],
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(knows),
        min_depth: 1,
        max_depth: 2,
        include_seed: false,
        max_visited: 32,
        max_edges: 64,
        result_limit: 32,
    };
    let filters = [
        QueryFilter::Graph(graph),
        QueryFilter::Scalar {
            index: indexes.active,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Point {
            index: indexes.position,
            predicate: PointFilter::Bbox(Bounds::new(0.0, 1.0, 0.0, 1.0).unwrap()),
        },
    ];
    let order = if approximate {
        QueryOrder::ApproximateVector {
            index: indexes.quantized,
            query: &[1.0, 0.0],
            metric: VectorMetric::Cosine,
            ef: 2,
        }
    } else {
        QueryOrder::ExactVector {
            index: indexes.vector,
            query: &[1.0, 0.0],
            metric: VectorMetric::Cosine,
        }
    };
    let mut query = db
        .prepare_query(QueryRequest {
            collection: people,
            filters: &filters,
            order,
            projection: Projection::Ids,
            total_limit: Some(2),
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    query
        .next_page(2, generous(), || false)
        .unwrap()
        .rows
        .into_iter()
        .map(|row| row.id)
        .collect()
}

#[test]
fn approximate_vector_filters_before_shortlist_pages_and_retries_with_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let f = create_fixture(&temp.path().join("db"));
    // p5 is globally tied for the best compact cosine score but does not match
    // `flood`. A global ef=2 shortlist would be [p0,p5] and return only p0
    // after filtering; filter-before-shortlist must return both p0 and p1.
    let flood = [QueryFilter::Text {
        index: f.indexes.text,
        query: "flood",
        matching: TextMatch::Any,
    }];
    let discriminating =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &flood,
            order: QueryOrder::ApproximateVector {
                index: f.indexes.quantized,
                query: &[1.0, 0.0],
                metric: VectorMetric::Cosine,
                ef: 2,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Order,
        })
        .unwrap()
        .next_page(2, generous(), || false)
        .unwrap();
    assert_eq!(
        discriminating
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![f.ids["p0"], f.ids["p1"]]
    );
    assert_eq!(discriminating.approximation.unwrap().examined, 2);

    let eligible = [
        (f.ids["p0"], [1.0, 0.0]),
        (f.ids["p1"], [1.0, 1.0]),
        (f.ids["p3"], [-1.0, 0.0]),
        (f.ids["p5"], [1.0, 0.0]),
    ];
    let expected = independent_quantized_oracle(&eligible, [1.0, 0.0], 3);
    let filters = [QueryFilter::Scalar {
        index: f.indexes.active,
        predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
    }];

    for driver in [
        CandidateDriver::Entities,
        CandidateDriver::Filter(0),
        CandidateDriver::Order,
        CandidateDriver::Auto,
    ] {
        let mut query =
            f.db.prepare_query(QueryRequest {
                collection: f.people,
                filters: &filters,
                order: QueryOrder::ApproximateVector {
                    index: f.indexes.quantized,
                    query: &[1.0, 0.0],
                    metric: VectorMetric::Cosine,
                    ef: 3,
                },
                projection: Projection::Ids,
                total_limit: None,
                driver,
            })
            .unwrap();

        let mut zero_lanes = generous();
        zero_lanes.vector_lanes = 0;
        assert!(matches!(
            query.next_page(2, zero_lanes, || false),
            Err(QueryError::BudgetExceeded {
                resource: WorkResource::VectorLanes,
                attempted: 2,
                ..
            })
        ));
        let mut calls = 0;
        assert!(matches!(
            query.next_page(2, generous(), || {
                calls += 1;
                calls == 4
            }),
            Err(QueryError::Cancelled)
        ));

        let first = query.next_page(2, generous(), || false).unwrap();
        let diagnostic = first.approximation.unwrap();
        assert_eq!(diagnostic.method, ApproxVectorMethod::SymmetricInt8ScanV1);
        assert_eq!(diagnostic.ef, 3);
        assert_eq!(diagnostic.examined, eligible.len());
        assert_eq!(diagnostic.reranked, 3);
        assert_eq!(first.work.vector_lanes, 20); // 4 compact + 3*(reencode+exact), dim=2
        assert_eq!(first.work.vector_sidecars, 3);
        assert!(!first.done);
        let second = query.next_page(2, generous(), || false).unwrap();
        assert!(second.done);
        assert_eq!(second.approximation, first.approximation);
        let actual = first
            .rows
            .iter()
            .chain(&second.rows)
            .map(|row| (row.id, cosine(&row.order)))
            .collect::<Vec<_>>();
        assert_eq!(
            actual.iter().map(|row| row.0).collect::<Vec<_>>(),
            expected.iter().map(|row| row.0).collect::<Vec<_>>()
        );
        for (actual, expected) in actual.iter().zip(&expected) {
            assert_eq!(actual.1.total_cmp(&expected.1), std::cmp::Ordering::Equal);
        }
        match driver {
            CandidateDriver::Order => {
                assert_eq!(
                    first.driver,
                    QueryDriver::QuantizedVector(f.indexes.quantized)
                );
                assert_eq!(first.work.vector_locators, 10); // 6 + terminal + 3 rechecks
            }
            CandidateDriver::Filter(_) | CandidateDriver::Auto => {
                assert_eq!(first.driver, QueryDriver::Scalar(f.indexes.active));
                assert_eq!(first.work.vector_locators, 7); // 4 point probes + 3 rechecks
            }
            CandidateDriver::Entities => assert_eq!(first.driver, QueryDriver::Entities),
        }
    }
}

#[test]
fn exact_vector_and_bm25_ties_remain_stable_across_page_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let f = create_fixture(&temp.path().join("db"));

    let mut exact =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &[],
            order: QueryOrder::ExactVector {
                index: f.indexes.vector,
                query: &[1.0, 0.0],
                metric: VectorMetric::Cosine,
            },
            projection: Projection::Ids,
            total_limit: Some(3),
            driver: CandidateDriver::Order,
        })
        .unwrap();
    // The sixth check is the exact scorer's first lane-chunk check: initial,
    // locator, candidate, sidecar and lane-budget checks precede it. A failed
    // page must leave search-after untouched for the retry below.
    let mut cancellation_checks = 0;
    assert!(matches!(
        exact.next_page(1, generous(), || {
            cancellation_checks += 1;
            cancellation_checks == 6
        }),
        Err(QueryError::Cancelled)
    ));
    let mut exact_rows = Vec::new();
    loop {
        let page = exact.next_page(1, generous(), || false).unwrap();
        exact_rows.extend(page.rows);
        if page.done {
            break;
        }
    }
    assert_eq!(
        exact_rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![f.ids["p0"], f.ids["p5"], f.ids["p1"]]
    );
    assert_eq!(cosine(&exact_rows[0].order).to_bits(), 0.0f64.to_bits());
    assert_eq!(cosine(&exact_rows[1].order).to_bits(), 0.0f64.to_bits());

    // p1 and p2 each contain road once in a two-token document, so their BM25
    // scores tie exactly and EntityId must carry the page boundary.
    let mut bm25 =
        f.db.prepare_query(QueryRequest {
            collection: f.people,
            filters: &[],
            order: QueryOrder::Bm25 {
                index: f.indexes.text,
                query: "road",
                matching: TextMatch::Any,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Order,
        })
        .unwrap();
    let first = bm25.next_page(1, generous(), || false).unwrap();
    assert!(!first.done);
    let second = bm25.next_page(1, generous(), || false).unwrap();
    assert!(second.done);
    assert_eq!(first.rows[0].id, f.ids["p1"]);
    assert_eq!(second.rows[0].id, f.ids["p2"]);
    let (OrderValue::Bm25(first_score), OrderValue::Bm25(second_score)) =
        (&first.rows[0].order, &second.rows[0].order)
    else {
        panic!("BM25 order values")
    };
    assert_eq!(
        first_score.total_cmp(second_score),
        std::cmp::Ordering::Equal
    );
}

#[derive(Clone, Copy, Debug)]
enum OrphanDriver {
    Scalar,
    Graph,
    Text,
    Spatial,
    ExactVector,
    QuantizedVector,
}

#[test]
fn every_native_driver_refuses_an_orphan_winner_without_full_candidate_rescoring() {
    let temp = tempfile::tempdir().unwrap();
    for case in [
        OrphanDriver::Scalar,
        OrphanDriver::Graph,
        OrphanDriver::Text,
        OrphanDriver::Spatial,
        OrphanDriver::ExactVector,
        OrphanDriver::QuantizedVector,
    ] {
        let path = temp.path().join(format!("orphan-{case:?}"));
        let Fixture {
            db,
            people,
            ids,
            indexes,
            knows,
        } = create_fixture(&path);
        let orphan = match case {
            OrphanDriver::Scalar => ids["p2"], // false sorts before true
            OrphanDriver::Graph => ids["p1"],  // first BFS result
            OrphanDriver::Text
            | OrphanDriver::Spatial
            | OrphanDriver::ExactVector
            | OrphanDriver::QuantizedVector => ids["p0"],
        };
        drop(db);
        let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
        assert!(raw.delete(&primary_key(orphan)).unwrap());
        raw.commit().unwrap();
        drop(raw);
        let db = Database::open_snapshot(&path, cfg()).unwrap();

        let result = match case {
            OrphanDriver::Scalar => db
                .prepare_query(QueryRequest {
                    collection: people,
                    filters: &[],
                    order: QueryOrder::Scalar {
                        index: indexes.active,
                        direction: e4_prototype::collections::SortDirection::Ascending,
                    },
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Order,
                })
                .unwrap()
                .next_page(1, generous(), || false),
            OrphanDriver::Graph => {
                let filters = [QueryFilter::Graph(BfsRequest {
                    seed: ids["p0"],
                    direction: Direction::Outgoing,
                    context: GraphContextId::BASE,
                    edge_type: Some(knows),
                    min_depth: 1,
                    max_depth: 2,
                    include_seed: false,
                    max_visited: 32,
                    max_edges: 64,
                    result_limit: 32,
                })];
                db.prepare_query(QueryRequest {
                    collection: people,
                    filters: &filters,
                    order: QueryOrder::EntityId,
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Filter(0),
                })
                .unwrap()
                .next_page(1, generous(), || false)
            }
            OrphanDriver::Text => {
                let filters = [QueryFilter::Text {
                    index: indexes.text,
                    query: "flood",
                    matching: TextMatch::Any,
                }];
                db.prepare_query(QueryRequest {
                    collection: people,
                    filters: &filters,
                    order: QueryOrder::EntityId,
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Filter(0),
                })
                .unwrap()
                .next_page(1, generous(), || false)
            }
            OrphanDriver::Spatial => {
                let filters = [QueryFilter::Point {
                    index: indexes.position,
                    predicate: PointFilter::Bbox(Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap()),
                }];
                db.prepare_query(QueryRequest {
                    collection: people,
                    filters: &filters,
                    order: QueryOrder::EntityId,
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Filter(0),
                })
                .unwrap()
                .next_page(1, generous(), || false)
            }
            OrphanDriver::ExactVector => db
                .prepare_query(QueryRequest {
                    collection: people,
                    filters: &[],
                    order: QueryOrder::ExactVector {
                        index: indexes.vector,
                        query: &[1.0, 0.0],
                        metric: VectorMetric::Cosine,
                    },
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Order,
                })
                .unwrap()
                .next_page(1, generous(), || false),
            OrphanDriver::QuantizedVector => db
                .prepare_query(QueryRequest {
                    collection: people,
                    filters: &[],
                    order: QueryOrder::ApproximateVector {
                        index: indexes.quantized,
                        query: &[1.0, 0.0],
                        metric: VectorMetric::Cosine,
                        ef: 3,
                    },
                    projection: Projection::Ids,
                    total_limit: None,
                    driver: CandidateDriver::Order,
                })
                .unwrap()
                .next_page(1, generous(), || false),
        };
        assert!(
            matches!(result, Err(QueryError::Database(Error::Corrupt(_)))),
            "{case:?}: {result:?}"
        );
    }
}

#[test]
fn one_snapshot_stays_old_while_committed_multifamily_mutation_reopens_new() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut f = create_fixture(&path);
    let old = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(
        mixed_ids(&old, f.people, &f.ids, f.indexes, f.knows, false),
        vec![f.ids["p1"]]
    );
    assert_eq!(
        mixed_ids(&old, f.people, &f.ids, f.indexes, f.knows, true),
        vec![f.ids["p1"]]
    );

    f.db.update(
        f.people,
        "p1",
        &json!({"age":35,"body":"forest","embedding":[0.0,1.0],"position":point(5.0,5.0)}),
    )
    .unwrap();
    f.db.unlink(f.ids["p0"], "knows", f.ids["p1"], "").unwrap();
    f.db.delete(f.people, "p2").unwrap();
    f.db.link(f.ids["p0"], "knows", f.ids["p5"], "", &json!({}))
        .unwrap();
    let new_p2 =
        f.db.put(
            f.people,
            "p2",
            &json!({
                "age":30,"active":false,"body":"river road","embedding":[0.0,1.0],
                "position":point(0.0,1.0),
                "profile":{"codes":[1,2,3],"nested":{"enabled":true}}
            }),
        )
        .unwrap();
    assert_ne!(new_p2, f.ids["p2"]);
    f.db.commit().unwrap();

    // The held reader owns one immutable database snapshot across every family.
    assert_eq!(
        mixed_ids(&old, f.people, &f.ids, f.indexes, f.knows, false),
        vec![f.ids["p1"]]
    );
    assert_eq!(
        mixed_ids(&old, f.people, &f.ids, f.indexes, f.knows, true),
        vec![f.ids["p1"]]
    );
    let reopened = Database::open_snapshot(&path, cfg()).unwrap();
    assert_eq!(
        mixed_ids(&reopened, f.people, &f.ids, f.indexes, f.knows, false),
        vec![f.ids["p5"]]
    );
    assert_eq!(
        mixed_ids(&reopened, f.people, &f.ids, f.indexes, f.knows, true),
        vec![f.ids["p5"]]
    );
}

#[test]
fn text_and_spatial_drivers_page_more_than_65536_matches_without_a_result_cap() {
    const ROWS: usize = 65_537;
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "many",
            vec![
                ("body".into(), Kind::Text),
                ("position".into(), Kind::Point),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let text = db.create_text_index(collection, "body", "body").unwrap();
    let spatial = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    assert!(db.build_index_step(text, 1).unwrap());
    assert!(db.build_index_step(spatial, 1).unwrap());
    db.commit().unwrap();
    let mut expected = Vec::with_capacity(ROWS);
    for position in 0..ROWS {
        expected.push(
            db.put(
                collection,
                &format!("row/{position:05}"),
                &json!({"body":"needle","position":point(0.0,0.0)}),
            )
            .unwrap(),
        );
        if position % 512 == 511 {
            db.commit().unwrap();
            db.checkpoint().unwrap();
        }
    }
    db.commit().unwrap();

    let filters = [QueryFilter::Text {
        index: text,
        query: "needle",
        matching: TextMatch::Any,
    }];
    let mut query = db
        .prepare_query(QueryRequest {
            collection,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let mut actual = Vec::with_capacity(ROWS);
    loop {
        let mut budget = generous();
        budget.candidates = ROWS as u64 + 1;
        budget.text_postings = ROWS as u64 + 1;
        let page = query.next_page(8192, budget, || false).unwrap();
        assert_eq!(page.driver, QueryDriver::Text(text));
        assert_eq!(page.work.text_postings, ROWS as u64 + 1);
        actual.extend(page.rows.into_iter().map(|row| row.id));
        if page.done {
            break;
        }
    }
    assert_eq!(actual, expected);

    let filters = [QueryFilter::Point {
        index: spatial,
        predicate: PointFilter::Bbox(Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap()),
    }];
    let mut query = db
        .prepare_query(QueryRequest {
            collection,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let mut actual = Vec::with_capacity(ROWS);
    loop {
        let mut budget = generous();
        budget.candidates = ROWS as u64 + 1;
        budget.spatial_postings = ROWS as u64 + 1;
        let page = query.next_page(8192, budget, || false).unwrap();
        assert_eq!(
            page.driver,
            QueryDriver::Spatial {
                index: spatial,
                fallback_world: true,
            }
        );
        assert_eq!(page.work.spatial_postings, ROWS as u64 + 1);
        actual.extend(page.rows.into_iter().map(|row| row.id));
        if page.done {
            break;
        }
    }
    assert_eq!(actual, expected);
}
