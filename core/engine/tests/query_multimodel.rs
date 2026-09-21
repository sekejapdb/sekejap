//! Combined-query acceptance over the deterministic Phase 2 app fixture.
//! Expected memberships and ranking math below are derived directly from the
//! fixture rather than from family query helpers.
use sekejap_core::{
    collections::{
        ApproxVectorMethod, BfsRequest, CandidateDriver, CollectionId, CollectionOptions, Database,
        Direction, EntityId, Error, Geom, GeometryFilter, GraphContextId, IndexId, OrderValue,
        PointFilter, ProjectedValue, Projection, QueryBudget, QueryDriver, QueryError, QueryFilter,
        QueryOrder, QueryRequest, ScalarFilter, ScalarValue, SortDirection, SpatialCandidates,
        TextMatch, VectorMetric, WorkResource,
        verification::{verify_indexed_source, VerificationLimits},
    },
    pagewal::PageWalStore,
    spatial_geometry,
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
    ops::Bound,
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
        key_postings: 10_000,
        rows_written: 10_000,
        groups: 10_000,
        output_bytes: 1 << 20,
        deadline: None,
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
    knows: sekejap_core::collections::EdgeTypeId,
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

fn graph_request(f: &Fixture) -> BfsRequest<'static> {
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
        edge_where: &[],
        node_where: &[],
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
) -> sekejap_core::collections::QueryPage {
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
) -> sekejap_core::collections::QueryPage {
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
    assert!(page.work.vector_sidecars > 0);
    assert!(page.work.vector_lanes > 0);
    // The non-driving point filter is answered from its own cover, walked
    // once per PREPARED QUERY rather than once per candidate, so the postings
    // are charged to the page that builds the set -- here the first call, the
    // one that then failed on primary reads. The set it left behind is whole
    // (a walk that cannot finish leaves `Overflow` or nothing, never a partial
    // set), so this page charges no postings and still applies the filter.
    assert_eq!(page.work.spatial_postings, 0);

    // The same filters on a query whose FIRST page succeeds: the cover walk
    // and the page it pays for are the same call, and the spatial postings
    // are charged there -- the point filter is applied from its postings,
    // before the ranked top-k, exactly as the graph and vector work is.
    let mut spatial_first =
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
    let built = spatial_first.next_page(2, generous(), || false).unwrap();
    assert!(built.work.spatial_postings > 0);
    assert_eq!(
        built.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![f.ids["p1"]]
    );

    // A budget that cannot afford one posting gets the row-read path back,
    // not a half-built set and not a new failure: one `SpatialPostings` per
    // candidate is what a non-driving point filter always cost.
    let mut no_postings = generous();
    no_postings.spatial_postings = 0;
    let mut starved =
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
    assert!(matches!(
        starved.next_page(2, no_postings, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::SpatialPostings,
            attempted: 1,
            ..
        })
    ));
    // ... and the same query, given the budget back, answers as it always did.
    assert_eq!(
        starved
            .next_page(2, generous(), || false)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![f.ids["p1"]]
    );

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
    // Zero: the text merge only hands over documents whose posting is live,
    // and a posting retires in the same transaction as its row, so a
    // text-driven key-only page no longer probes the primary tree per winner
    // (it was 1, the winner existence probe).
    assert_eq!(first.work.primary_reads, 0);
    assert!(!first.done);
    let second = query.next_page(1, generous(), || false).unwrap();
    assert_eq!(second.rows[0].id, f.ids["p1"]);
    // Two, and they are this page's own. The text merge ascends by document
    // and an id ranking wants exactly that order, so page one STOPPED on a
    // full heap instead of walking the merge to its end, and page two RESUMES
    // by seeking every term stream to the document page one ended on. What it
    // probes is the resumed document -- re-emitted once and dropped by the
    // continuation key -- and the one it returns. Nothing was ranked twice,
    // and nothing past this page has been touched. The winner's existence
    // check is still owed and still paid.
    assert_eq!(second.work.scalar_postings, 2);
    assert_eq!(second.work.primary_reads, 0);
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
    knows: sekejap_core::collections::EdgeTypeId,
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
        edge_where: &[],
        node_where: &[],
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
        // Two plans answer this request. The per-candidate path (entity or
        // scalar driver) decodes 4 compact entries and, for each of the 3
        // rerank winners, re-encodes and scores the exact vector: 4*2 +
        // 3*(2+2) = 20 lanes. The page-order compact scan (quantized driver)
        // scores the same 4 entries and reranks the 3 winners from the f32
        // sidecar without re-encoding: 4*2 + 3*2 = 14 lanes.
        let scan_plan = matches!(first.driver, QueryDriver::QuantizedVector(_));
        assert_eq!(
            first.work.vector_lanes,
            if scan_plan { 14 } else { 20 },
            "{:?} {:?}",
            first.driver,
            first.work
        );
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
            // Auto joins Order on the scan plan: 4 of 6 rows pass the equality,
            // far above the 1-in-16 density at which point-getting the matches
            // would beat one page-order pass over the compact entries.
            CandidateDriver::Order | CandidateDriver::Auto => {
                assert_eq!(
                    first.driver,
                    QueryDriver::QuantizedVector(f.indexes.quantized)
                );
                // 6 compact entries read, each charged as one locator; the
                // rerank's locator validations are covered by the entry read
                // that carried the locator inline.
                assert_eq!(first.work.vector_locators, 6, "{:?}", first.work);
            }
            CandidateDriver::Filter(_) => {
                assert_eq!(first.driver, QueryDriver::Scalar(f.indexes.active));
                assert_eq!(first.work.vector_locators, 7); // 4 point probes + 3 rechecks
            }
            CandidateDriver::Entities => assert_eq!(first.driver, QueryDriver::Entities),
            CandidateDriver::Keys => unreachable!("this test's driver list never includes Keys"),
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
    // The sixth check is the one the page's first row is sized by, inside the
    // emit stage: the page-entry check, the scan's candidate, sidecar and lane
    // charges, and the winner's existence read precede it. A failed page must
    // leave search-after -- and every other resume state, the ranked rows the
    // page held back included -- untouched for the retry below.
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
                        direction: sekejap_core::collections::SortDirection::Ascending,
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
                    edge_where: &[],
                    node_where: &[],
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
        match case {
            // A text merge and a spatial cell walk only hand over postings
            // that retire in the same transaction as their row, so a key-only
            // page driven by either no longer probes the primary tree per
            // winner (item T3). A store-level orphan -- a row removed behind
            // the index's back, which no supported write can do -- is
            // therefore not refused by the page; it is the verifier's to
            // report, and it must.
            OrphanDriver::Text | OrphanDriver::Spatial => {
                assert!(result.is_ok(), "{case:?}: {result:?}");
                drop(db);
                let mut issues = Vec::new();
                let report =
                    verify_indexed_source(&path, VerificationLimits::default(), |issue| {
                        issues.push(issue.clone())
                    })
                    .unwrap();
                assert!(report.complete && !report.clean, "{case:?}: {report:?}");
                assert!(
                    issues.iter().any(|issue| issue.entity == Some(orphan)),
                    "{case:?}: the verifier did not name the orphan: {issues:?}"
                );
            }
            _ => assert!(
                matches!(result, Err(QueryError::Database(Error::Corrupt(_)))),
                "{case:?}: {result:?}"
            ),
        }
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
    let mut postings = 0;
    let mut pages = 0u64;
    loop {
        let mut budget = generous();
        budget.candidates = ROWS as u64 + 1;
        budget.text_postings = ROWS as u64 + 1;
        let page = query.next_page(8192, budget, || false).unwrap();
        assert_eq!(page.driver, QueryDriver::Text(text));
        postings += page.work.text_postings;
        pages += 1;
        actual.extend(page.rows.into_iter().map(|row| row.id));
        if page.done {
            break;
        }
    }
    assert_eq!(actual, expected);
    // The merge order (ascending sequence) IS the ranking's order, so each
    // page walks its own slice and resumes where the last one stopped: one
    // pass over the posting range for the whole answer, plus the resumed
    // document each page re-emits and the end-of-tier charge each page pays.
    assert!(
        postings <= ROWS as u64 + 4 * pages,
        "{pages} pages over {ROWS} matches read {postings} text postings. The \
         posting range is walked ONCE for the whole answer (at most {}); \
         re-opening it per page is {}",
        ROWS as u64 + 4 * pages,
        (ROWS as u64 + 1) * pages
    );

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
    let mut postings = 0;
    let mut pages = 0u64;
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
        postings += page.work.spatial_postings;
        pages += 1;
        actual.extend(page.rows.into_iter().map(|row| row.id));
        if page.done {
            break;
        }
    }
    assert_eq!(actual, expected);
    assert_eq!(
        postings,
        ROWS as u64 + 1,
        "{pages} pages over {ROWS} matches read {postings} spatial postings;          the cell walk is opened once for the whole answer ({}), not {} times",
        ROWS as u64 + 1,
        pages
    );
}

// ---------------------------------------------------------------------------
// The packed norm tier, seen from the query executor.
//
// A text index built AFTER its corpus packs its document lengths into `0x7B`
// blocks (256 documents per block) and writes no `0x76` head row. Every reader
// of a document length therefore has to look at the head row first and fall
// back to the block. `text_indexes::read_norm_cached` does. The executor's own
// scorer has to as well, or every scored document silently vanishes: BM25
// ranks nothing, a text filter that is not the candidate driver matches
// nothing, and a phrase never matches. Only a DRIVING `Any`/`All` text filter
// escapes, because the cursor answers it without scoring.
// ---------------------------------------------------------------------------

/// Deterministic prose, ASCII only, so the oracle below can tokenize it with
/// the same rule the analyzer uses: runs of alphanumerics, lowercased.
fn packed_doc(i: u64) -> String {
    let subject = ["harbour", "mill", "terrace", "orchard", "quarry"][(i % 5) as usize];
    let verb = ["flooded", "settled", "burned", "drained"][(i % 4) as usize];
    let rare = if i % 97 == 0 { " comet" } else { "" };
    format!("the {subject} river {verb} again in the year {i}{rare}")
}

fn packed_tokens(text: &str) -> Vec<String> {
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

fn packed_features(path: &Path) -> u64 {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let header = raw.get(&[0, 0, 0]).unwrap().unwrap();
    assert_eq!(&header[..8], b"E4COLL2\0");
    u64::from_be_bytes(header[10 + 8..10 + 16].try_into().unwrap())
}

/// Does any `0x7B` norm block exist for this index?
fn packed_norm_blocks(path: &Path, index: IndexId) -> usize {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut prefix = vec![0x7b_u8];
    prefix.extend(ordered(index.0));
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

fn packed_norm_head_rows(path: &Path, index: IndexId) -> usize {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut prefix = vec![0x76_u8];
    prefix.extend(ordered(index.0));
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

fn packed_budget() -> QueryBudget {
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
        rows_written: 1 << 22,
        groups: 1 << 22,
        output_bytes: 1 << 24,
        deadline: None,
    }
}

/// An in-memory BM25, written from the published constants rather than from
/// the engine's own scorer: K1 = 1.2, B = 0.75, and the Robertson/Lucene idf.
fn packed_bm25_oracle(corpus: &[Vec<String>], doc: usize, terms: &[&str]) -> Option<f64> {
    let n = corpus.len() as f64;
    let total: f64 = corpus.iter().map(|tokens| tokens.len() as f64).sum();
    let average = total / n;
    let length = corpus[doc].len() as f64;
    let mut score = 0.0;
    let mut hit = false;
    for term in terms {
        let tf = corpus[doc].iter().filter(|token| *token == term).count() as f64;
        if tf == 0.0 {
            continue;
        }
        hit = true;
        let df = corpus
            .iter()
            .filter(|tokens| tokens.iter().any(|token| token == term))
            .count() as f64;
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        score += idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * length / average));
    }
    hit.then_some(score)
}

const PACKED_ROWS: u64 = 700;

#[test]
fn a_packed_norm_tier_still_scores_text_in_the_query_executor() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("packed_norms");

    // Corpus first, text index afterwards: the late build takes the packed
    // path and writes norms as `0x7B` blocks only.
    let (collection, bucket_index, text_index, ids) = {
        let mut db = Database::create(&path, cfg()).unwrap();
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
        for i in 0..PACKED_ROWS {
            ids.push(
                db.put(
                    docs,
                    &format!("d{i:05}"),
                    &json!({ "body": packed_doc(i), "bucket": (i % 4) as i64 }),
                )
                .unwrap(),
            );
            if i % 256 == 255 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();

        let text = db.create_text_index(docs, "body", "body").unwrap();
        db.commit().unwrap();
        db.build_index_to_ready(text, 256).unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        (docs, bucket, text, ids)
    };

    // The fixture proves nothing unless the build really packed.
    assert_eq!(
        packed_features(&path) & 0x40,
        0x40,
        "the late text build did not set the packed feature bit"
    );
    assert!(
        packed_norm_blocks(&path, text_index) >= 3,
        "fewer than three 0x7B norm blocks: {}",
        packed_norm_blocks(&path, text_index)
    );
    assert_eq!(
        packed_norm_head_rows(&path, text_index),
        0,
        "a packed build must not also write 0x76 norm head rows"
    );

    let db = Database::open(&path, cfg()).unwrap();
    let corpus: Vec<Vec<String>> = (0..PACKED_ROWS)
        .map(|i| packed_tokens(&packed_doc(i)))
        .collect();
    let bucket_one = |i: u64| i % 4 == 1;

    // (a) BM25 order behind a scalar driver. The cursor is the scalar index,
    //     so the executor has to score every candidate itself.
    let scalar_one = [QueryFilter::Scalar {
        index: bucket_index,
        predicate: ScalarFilter::Eq(ScalarValue::I64(1)),
    }];
    let mut bm25 = db
        .prepare_query(QueryRequest {
            collection,
            filters: &scalar_one,
            order: QueryOrder::Bm25 {
                index: text_index,
                query: "harbour comet",
                matching: TextMatch::Any,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let page = bm25.next_page(256, packed_budget(), || false).unwrap();
    assert_eq!(page.driver, QueryDriver::Scalar(bucket_index));
    let mut scored: BTreeMap<EntityId, f64> = BTreeMap::new();
    for row in &page.rows {
        let OrderValue::Bm25(score) = row.order else {
            panic!("BM25 order value");
        };
        scored.insert(row.id, score);
    }
    let mut expected_bm25: BTreeMap<EntityId, f64> = BTreeMap::new();
    for i in 0..PACKED_ROWS {
        if !bucket_one(i) {
            continue;
        }
        if let Some(score) = packed_bm25_oracle(&corpus, i as usize, &["harbour", "comet"]) {
            expected_bm25.insert(ids[i as usize], score);
        }
    }
    assert_eq!(
        scored.keys().copied().collect::<Vec<_>>(),
        expected_bm25.keys().copied().collect::<Vec<_>>(),
        "BM25 behind a scalar driver lost documents"
    );
    for (id, expected) in &expected_bm25 {
        let actual = scored[id];
        assert!(
            (actual - expected).abs() <= 1e-12,
            "BM25 score for {id:?}: {actual} vs oracle {expected}"
        );
    }

    // (b) A text filter that is NOT the candidate driver.
    let scalar_then_text = [
        QueryFilter::Scalar {
            index: bucket_index,
            predicate: ScalarFilter::Eq(ScalarValue::I64(1)),
        },
        QueryFilter::Text {
            index: text_index,
            query: "comet",
            matching: TextMatch::Any,
        },
    ];
    let mut passenger = db
        .prepare_query(QueryRequest {
            collection,
            filters: &scalar_then_text,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let page = passenger.next_page(256, packed_budget(), || false).unwrap();
    assert_eq!(page.driver, QueryDriver::Scalar(bucket_index));
    let expected_passenger: Vec<EntityId> = (0..PACKED_ROWS)
        .filter(|i| bucket_one(*i) && corpus[*i as usize].iter().any(|token| token == "comet"))
        .map(|i| ids[i as usize])
        .collect();
    assert!(
        expected_passenger.len() >= 2,
        "the oracle expected too few rows to prove anything"
    );
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        expected_passenger,
        "a non-driving text filter lost documents"
    );

    // (c) A phrase. Phrase refinement always scores, driver or not.
    let phrase = [QueryFilter::Text {
        index: text_index,
        query: "river flooded again",
        matching: TextMatch::Phrase,
    }];
    let mut phrases = db
        .prepare_query(QueryRequest {
            collection,
            filters: &phrase,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let mut actual_phrase = Vec::new();
    loop {
        let page = phrases.next_page(256, packed_budget(), || false).unwrap();
        let done = page.done;
        actual_phrase.extend(page.rows.into_iter().map(|row| row.id));
        if done {
            break;
        }
    }
    let wanted = ["river", "flooded", "again"];
    let expected_phrase: Vec<EntityId> = (0..PACKED_ROWS)
        .filter(|i| {
            corpus[*i as usize]
                .windows(3)
                .any(|window| window.iter().zip(wanted).all(|(token, term)| token == term))
        })
        .map(|i| ids[i as usize])
        .collect();
    assert!(
        expected_phrase.len() >= 100,
        "the oracle expected too few phrase rows to prove anything"
    );
    assert_eq!(
        actual_phrase, expected_phrase,
        "a phrase query lost documents"
    );

    // (d) The SAME BM25, this time driven by the text merge cursor itself.
    //     The scorer now takes the frequencies the merge decoded instead of
    //     re-reading them, so this is where that shortcut has to produce
    //     byte-identical scores -- against the oracle, and against (a), which
    //     reached them the long way behind a scalar driver.
    let mut expected_text_driven: Vec<(EntityId, f64)> = (0..PACKED_ROWS)
        .filter_map(|i| {
            packed_bm25_oracle(&corpus, i as usize, &["harbour", "comet"])
                .map(|score| (ids[i as usize], score))
        })
        .collect();
    expected_text_driven.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.sequence.cmp(&right.0.sequence))
    });
    assert!(
        expected_text_driven.len() >= 140,
        "the oracle expected too few BM25 rows to prove anything"
    );

    let text_any = [QueryFilter::Text {
        index: text_index,
        query: "harbour comet",
        matching: TextMatch::Any,
    }];
    for driver in [CandidateDriver::Auto, CandidateDriver::Filter(0)] {
        let mut driven = db
            .prepare_query(QueryRequest {
                collection,
                filters: &text_any,
                order: QueryOrder::Bm25 {
                    index: text_index,
                    query: "harbour comet",
                    matching: TextMatch::Any,
                },
                projection: Projection::Ids,
                total_limit: None,
                driver,
            })
            .unwrap();
        let page = driven
            .next_page(PACKED_ROWS as usize, packed_budget(), || false)
            .unwrap();
        assert_eq!(page.driver, QueryDriver::Text(text_index));
        assert!(page.done);
        let actual: Vec<(EntityId, f64)> = page
            .rows
            .iter()
            .map(|row| {
                let OrderValue::Bm25(score) = row.order else {
                    panic!("BM25 order value");
                };
                (row.id, score)
            })
            .collect();
        assert_eq!(
            actual.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            expected_text_driven
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            "text-driven BM25 order ({driver:?})"
        );
        for ((id, score), (_, oracle)) in actual.iter().zip(&expected_text_driven) {
            assert!(
                (score - oracle).abs() <= 1e-12,
                "text-driven BM25 score for {id:?}: {score} vs oracle {oracle}"
            );
            // And identical to what the scalar-driven query in (a) produced
            // for the documents the two queries share.
            if let Some(scalar_score) = scored.get(id) {
                assert_eq!(
                    score.to_bits(),
                    scalar_score.to_bits(),
                    "the same document scored differently behind a different driver"
                );
            }
        }
    }

    // (e) A text driver over DIFFERENT terms from the order it feeds. The
    //     driver decodes the frequency of "comet"; ranking wants "harbour
    //     comet". Taking the driver's frequencies here would score the wrong
    //     words, so the scorer must still read its own.
    let comet_only = [QueryFilter::Text {
        index: text_index,
        query: "comet",
        matching: TextMatch::Any,
    }];
    let mut mixed = db
        .prepare_query(QueryRequest {
            collection,
            filters: &comet_only,
            order: QueryOrder::Bm25 {
                index: text_index,
                query: "harbour comet",
                matching: TextMatch::Any,
            },
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let page = mixed
        .next_page(PACKED_ROWS as usize, packed_budget(), || false)
        .unwrap();
    assert_eq!(page.driver, QueryDriver::Text(text_index));
    let mut expected_mixed: Vec<(EntityId, f64)> = (0..PACKED_ROWS)
        .filter(|i| corpus[*i as usize].iter().any(|token| token == "comet"))
        .filter_map(|i| {
            packed_bm25_oracle(&corpus, i as usize, &["harbour", "comet"])
                .map(|score| (ids[i as usize], score))
        })
        .collect();
    expected_mixed.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.sequence.cmp(&right.0.sequence))
    });
    assert!(expected_mixed.len() >= 7);
    for (row, (id, oracle)) in page.rows.iter().zip(&expected_mixed) {
        let OrderValue::Bm25(score) = row.order else {
            panic!("BM25 order value");
        };
        assert_eq!(row.id, *id, "mixed-term BM25 order");
        assert!(
            (score - oracle).abs() <= 1e-12,
            "mixed-term BM25 score for {id:?}: {score} vs oracle {oracle}"
        );
    }
}

// ── the guarantee that stands in for the winner-stage existence probe ───────

const TOMBSTONE_ROWS: u64 = 1_000;
const TOMBSTONE_TERM: &str = "pipeline";

/// Nine documents in ten carry the term, so its whole posting list is one
/// packed `0x7A` segment and a delete cannot cheaply cut one document out of
/// it.
fn tombstone_body(i: u64) -> String {
    if i % 10 == 0 {
        return format!("the aqueduct carried water in the year {i}");
    }
    format!("the {TOMBSTONE_TERM} carried water in the year {i}")
}

fn text_prefix(tag: u8, index: IndexId, term: Option<&str>) -> Vec<u8> {
    let mut key = vec![tag];
    key.extend(ordered(index.0));
    if let Some(term) = term {
        key.extend(term.as_bytes());
        key.push(0);
    }
    key
}

fn raw_rows(path: &Path, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut out = Vec::new();
    for row in raw.range(prefix).unwrap() {
        let (key, value) = row.unwrap();
        if !key.starts_with(prefix) {
            break;
        }
        out.push((key, value));
    }
    out
}

fn text_ids(
    db: &Database,
    collection: CollectionId,
    index: IndexId,
    query: &str,
    matching: TextMatch,
    order: QueryOrder,
) -> Vec<EntityId> {
    let filters = [QueryFilter::Text {
        index,
        query,
        matching,
    }];
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection,
            filters: &filters,
            order,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut ids = Vec::new();
    loop {
        let page = prepared.next_page(4096, generous(), || false).unwrap();
        ids.extend(page.rows.iter().map(|row| row.id));
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    ids
}

/// A deleted document whose posting is still inside a packed segment is never
/// returned by any text-driven page.
///
/// The packed tier (`0x7A`, loop 4) does not remove a deleted document's
/// posting -- a delete will not rewrite a block -- so it records the
/// retirement at the head instead: `tf = 0` for the posting, the EMPTY value
/// for the `0x76` norm. Two readers make that record binding, and this pins
/// both:
///
/// * the merge cursor cancels a segment posting against its `tf = 0` head row,
///   so a deleted document is never even a candidate; and
/// * the scorer reads the document's norm before it scores it, and the EMPTY
///   head norm decodes as "not in the index", so `text_score` returns `None`
///   and the candidate is dropped before it can be a winner.
///
/// The second of those is why a BM25-ranked key-only page no longer goes back
/// to the primary tree to prove each winner present: the norm lookup already
/// did. This test is that proof, and it must hold before and after the probe
/// is removed.
#[test]
fn a_deleted_document_with_a_packed_posting_is_never_returned_by_a_text_page() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("packed-delete");
    let mut db = Database::create(&path, cfg()).unwrap();
    let docs = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    let mut ids = Vec::new();
    for i in 0..TOMBSTONE_ROWS {
        ids.push(
            db.put(docs, &format!("d{i:05}"), &json!({ "body": tombstone_body(i) }))
                .unwrap(),
        );
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    // Corpus first, index afterwards: the late build packs.
    let index = db.create_text_index(docs, "body_idx", "body").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(index, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);

    let segments = raw_rows(&path, &text_prefix(0x7a, index, Some(TOMBSTONE_TERM))).len();
    assert!(
        segments > 0,
        "the late build wrote no packed segment, so this test proves nothing"
    );
    assert_eq!(
        raw_rows(&path, &text_prefix(0x75, index, None)).len(),
        0,
        "a packed build must leave the head posting tier empty"
    );

    let victim = 501u64;
    assert_ne!(victim % 10, 0, "the deleted document must carry the term");
    let deleted = ids[victim as usize];
    let mut db = Database::open(&path, cfg()).unwrap();
    db.delete(docs, &format!("d{victim:05}")).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);

    // The delete did NOT touch the packed posting: it is still there, naming a
    // document that no longer exists.
    assert_eq!(
        raw_rows(&path, &text_prefix(0x7a, index, Some(TOMBSTONE_TERM))).len(),
        segments,
        "the delete rewrote a packed segment; the tombstone path is what is under test"
    );
    let mut posting_key = text_prefix(0x75, index, Some(TOMBSTONE_TERM));
    posting_key.extend(ordered(deleted.sequence));
    let posting = raw_rows(&path, &posting_key);
    assert_eq!(
        posting.len(),
        1,
        "the delete left no head posting row over the packed one"
    );
    assert_eq!(
        posting[0].1,
        0u32.to_be_bytes().to_vec(),
        "the head posting row is not the `tf = 0` tombstone"
    );
    let mut norm_key = text_prefix(0x76, index, None);
    norm_key.extend(ordered(deleted.sequence));
    let norm = raw_rows(&path, &norm_key);
    assert_eq!(norm.len(), 1, "the delete left no head norm row");
    assert!(
        norm[0].1.is_empty(),
        "the head norm row is not the EMPTY tombstone: {:?}",
        norm[0].1
    );

    // And the primary row really is gone, so nothing below is answered by it.
    let db = Database::open_snapshot(&path, cfg()).unwrap();
    assert!(raw_rows(&path, &primary_key(deleted)).is_empty());

    let live = (0..TOMBSTONE_ROWS).filter(|i| i % 10 != 0).count() - 1;

    // (1) A BM25-ranked page: every winner passed through the scorer.
    let ranked = text_ids(
        &db,
        docs,
        index,
        TOMBSTONE_TERM,
        TextMatch::Any,
        QueryOrder::Bm25 {
            index,
            query: TOMBSTONE_TERM,
            matching: TextMatch::Any,
        },
    );
    assert_eq!(ranked.len(), live, "bm25 page returned the wrong count");
    assert!(
        !ranked.contains(&deleted),
        "a bm25 page returned a deleted document"
    );

    // (2) An EntityId-ordered text-driven page, and the Any / All / Phrase
    //     filter shapes: the merge cursor is what refuses the document here.
    for (query, matching) in [
        (TOMBSTONE_TERM, TextMatch::Any),
        ("pipeline carried", TextMatch::All),
        ("pipeline carried", TextMatch::Phrase),
    ] {
        let ids = text_ids(&db, docs, index, query, matching, QueryOrder::EntityId);
        assert_eq!(
            ids.len(),
            live,
            "{matching:?} `{query}` returned the wrong count"
        );
        assert!(
            !ids.contains(&deleted),
            "{matching:?} `{query}` returned a deleted document"
        );
        assert!(
            ids.windows(2).all(|pair| pair[0].sequence < pair[1].sequence),
            "{matching:?} `{query}` is not in entity order"
        );
    }
}

/// Item T3: a spatial-driven key-only page no longer probes the primary tree
/// per winner. The guarantee that replaces the probe is that a point's cell
/// posting retires in the same transaction as its row, so a deleted row can
/// never be handed over by the cell walk -- under entity order and under the
/// driver's own cell order alike -- and the verifier stays clean.
#[test]
fn a_deleted_point_is_never_returned_by_a_spatial_page_that_no_longer_probes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut f = create_fixture(&path);
    let gone = f.ids["p3"]; // position (2.0, 2.0)
    f.db.delete(f.people, "p3").unwrap();
    f.db.commit().unwrap();
    f.db.checkpoint().unwrap();
    let people = f.people;
    let position = f.indexes.position;
    drop(f);

    let mut issues = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        issues.push(issue.clone())
    })
    .unwrap();
    assert!(report.complete && report.clean, "{report:?} {issues:?}");

    let db = Database::open_snapshot(&path, cfg()).unwrap();
    let filters = [QueryFilter::Point {
        index: position,
        predicate: PointFilter::Bbox(Bounds::new(-1.0, 5.0, -1.0, 5.0).unwrap()),
    }];
    let mut by_id = db
        .prepare_query(QueryRequest {
            collection: people,
            filters: &filters,
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = by_id.next_page(100, generous(), || false).unwrap();
    assert!(page.done);
    let ids_in_order: Vec<EntityId> = page.rows.iter().map(|row| row.id).collect();
    assert!(!ids_in_order.contains(&gone), "{ids_in_order:?}");
    assert!(!ids_in_order.is_empty());
    // The whole point of T3: the page proved nothing through the primary tree.
    assert_eq!(page.work.primary_reads, 0, "{:?}", page.work);

    let mut by_cell = db
        .prepare_query(QueryRequest {
            collection: people,
            filters: &filters,
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = by_cell.next_page(100, generous(), || false).unwrap();
    assert!(page.done);
    let mut ids_by_cell: Vec<EntityId> = page.rows.iter().map(|row| row.id).collect();
    ids_by_cell.sort();
    assert_eq!(ids_by_cell, ids_in_order);
    assert_eq!(page.work.primary_reads, 0, "{:?}", page.work);
}

/// Oracle parity for item KD: `CandidateDriver::Keys` under `QueryOrder::
/// Driver` must return exactly the entities an independent oracle gets by
/// enumerating them (`CandidateDriver::Entities`, any order) and sorting the
/// result by external key.
///
/// The fixture's keys are deliberately the REVERSE of insertion/id order
/// (`m{:05}` counting down as the loop counts up) rather than the zero-padded
/// ascending keys popsim happens to use -- see `ROOTCAUSE-count-all.md`'s own
/// warning that popsim's key order coinciding with id order is "a property of
/// the fixture, not of the engine". A test that passed only because key order
/// and id order agreed would not be testing the driver at all.
#[test]
fn keys_driver_order_matches_an_entity_enumeration_sorted_by_key() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "kd_oracle",
            vec![("v".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    const ROWS: u64 = 300;
    for i in 1..=ROWS {
        let key = format!("m{:05}", ROWS + 1 - i);
        db.put(rows, &key, &json!({"v": i as i64})).unwrap();
        if i % 97 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    // Delete some so the oracle also has to agree about absence.
    for i in (11..=ROWS).step_by(11) {
        let key = format!("m{:05}", ROWS + 1 - i);
        assert!(db.delete(rows, &key).unwrap());
    }
    db.commit().unwrap();

    let mut entities = db
        .prepare_query(QueryRequest {
            collection: rows,
            filters: &[],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Entities,
        })
        .unwrap();
    let mut by_key: Vec<(String, EntityId)> = Vec::new();
    loop {
        let page = entities.next_page(4096, generous(), || false).unwrap();
        for row in &page.rows {
            let entity = db.get_by_id(row.id).unwrap().unwrap();
            by_key.push((entity.key, row.id));
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    by_key.sort();
    let oracle: Vec<EntityId> = by_key.into_iter().map(|(_, id)| id).collect();

    let mut keys_query = db
        .prepare_query(QueryRequest {
            collection: rows,
            filters: &[],
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Keys,
        })
        .unwrap();
    let mut got = Vec::new();
    loop {
        let page = keys_query.next_page(4096, generous(), || false).unwrap();
        assert_eq!(page.driver, QueryDriver::Keys);
        assert_eq!(page.work.primary_reads, 0, "{:?}", page.work);
        got.extend(page.rows.iter().map(|row| row.id));
        if page.done || page.rows.is_empty() {
            break;
        }
    }

    assert!(got.len() > 200, "the fixture must have a real answer after deletes");
    assert_eq!(got.len(), oracle.len());
    assert_eq!(
        got, oracle,
        "the keys driver's own order must equal entities sorted by key"
    );
}

// -- QD: QueryOrder::Distance ----------------------------------------------

fn distance_order(index: IndexId, center: Point) -> QueryOrder<'static> {
    QueryOrder::Distance {
        index,
        center,
        direction: SortDirection::Ascending,
    }
}

fn drain_distance(
    db: &Database,
    collection: CollectionId,
    filters: &[QueryFilter<'_>],
    index: IndexId,
    center: Point,
    k: Option<usize>,
    page_size: usize,
    driver: CandidateDriver,
) -> (Vec<EntityId>, Vec<f64>, QueryDriver, u64) {
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection,
            filters,
            order: distance_order(index, center),
            projection: Projection::Ids,
            total_limit: k,
            driver,
        })
        .unwrap();
    let mut ids = Vec::new();
    let mut distances = Vec::new();
    let mut primary_reads = 0u64;
    let mut driver_seen = None;
    loop {
        let page = prepared.next_page(page_size, generous(), || false).unwrap();
        driver_seen = Some(page.driver);
        primary_reads += page.work.primary_reads;
        for row in &page.rows {
            ids.push(row.id);
            match &row.order {
                OrderValue::Distance(metres) => distances.push(*metres),
                other => panic!("distance order returned {other:?}"),
            }
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    (
        ids,
        distances,
        driver_seen.expect("a prepared query produces a page"),
        primary_reads,
    )
}

fn next_u64(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    *seed
}

fn unit(seed: &mut u64) -> f64 {
    (next_u64(seed) >> 11) as f64 / (1u64 << 53) as f64
}

/// (a) `QueryOrder::Distance` with total_limit k returns exactly
/// `query_point_nearest`'s ids, in the same order, across page sizes.
#[test]
fn distance_order_matches_query_point_nearest_across_page_sizes() {
    const BBOX_LON: (f64, f64) = (106.4, 108.8);
    const BBOX_LAT: (f64, f64) = (-7.8, -5.9);
    const CENTER: (f64, f64) = (107.6, -6.9);
    const ROWS: usize = 400;
    const K: usize = 15;

    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("position".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut seed = 0x51ea_d15c_0de_u64;
    for n in 0..ROWS {
        let lon = BBOX_LON.0 + (BBOX_LON.1 - BBOX_LON.0) * unit(&mut seed);
        let lat = BBOX_LAT.0 + (BBOX_LAT.1 - BBOX_LAT.0) * unit(&mut seed);
        db.put(
            collection,
            &format!("p{n:05}"),
            &json!({"position": point(lon, lat)}),
        )
        .unwrap();
        if n % 64 == 63 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let index = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    finish_build(&mut db, index);
    db.checkpoint().unwrap();

    let center = Point::new(CENTER.0, CENTER.1).unwrap();
    let oracle = db
        .query_point_nearest(index, center, K, SpatialCandidates::All, ROWS * 2, || false)
        .unwrap();
    assert_eq!(oracle.len(), K);
    let oracle_ids: Vec<EntityId> = oracle.iter().map(|hit| hit.id).collect();

    for page_size in [1, 3, 7, 50] {
        let (ids, distances, driver, primary_reads) = drain_distance(
            &db,
            collection,
            &[],
            index,
            center,
            Some(K),
            page_size,
            CandidateDriver::Auto,
        );
        assert_eq!(
            driver,
            QueryDriver::Nearest { index },
            "page_size {page_size}"
        );
        assert_eq!(ids, oracle_ids, "page_size {page_size}");
        assert_eq!(ids.len(), K, "page_size {page_size}");
        assert_eq!(primary_reads, 0, "page_size {page_size}: {primary_reads}");
        assert!(
            distances.windows(2).all(|pair| pair[0] <= pair[1]),
            "page_size {page_size}: distances not ascending: {distances:?}"
        );
        for (got, want) in distances.iter().zip(oracle.iter()) {
            assert_eq!(got.to_bits(), want.distance_metres.to_bits());
        }
    }
}

/// (b) A radius filter on the same index yields the nearest k WITHIN the
/// radius, and an empty page when nothing is within.
#[test]
fn distance_order_with_a_same_index_radius_is_nearest_within() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("position".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();
    // A tight cluster around the origin and one far point.
    let mut near = Vec::new();
    for n in 0..8 {
        near.push(
            db.put(
                collection,
                &format!("n{n}"),
                &json!({"position": point(n as f64 * 0.001, 0.0)}),
            )
            .unwrap(),
        );
    }
    db.put(
        collection,
        "far",
        &json!({"position": point(10.0, 10.0)}),
    )
    .unwrap();
    db.commit().unwrap();
    let index = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    finish_build(&mut db, index);

    let center = Point::new(0.0, 0.0).unwrap();
    let radius_metres = 500.0;
    let filters = [QueryFilter::Point {
        index,
        predicate: PointFilter::Radius {
            center,
            radius_metres,
        },
    }];
    let (ids, _, driver, primary_reads) = drain_distance(
        &db,
        collection,
        &filters,
        index,
        center,
        Some(3),
        10,
        CandidateDriver::Auto,
    );
    assert_eq!(driver, QueryDriver::Nearest { index });
    assert_eq!(primary_reads, 0);
    assert_eq!(ids, near[..3]);

    let empty_filters = [QueryFilter::Point {
        index,
        predicate: PointFilter::Radius {
            center: Point::new(40.0, 40.0).unwrap(),
            radius_metres: 50.0,
        },
    }];
    let (ids, _, _, _) = drain_distance(
        &db,
        collection,
        &empty_filters,
        index,
        Point::new(40.0, 40.0).unwrap(),
        Some(10),
        10,
        CandidateDriver::Auto,
    );
    assert!(ids.is_empty(), "nothing is within the empty radius: {ids:?}");
}

/// (c) A non-spatial born-range filter (C1's set) and a text filter as the
/// driver both agree with a brute-force oracle sorted by distance then id.
#[test]
fn distance_order_with_born_range_or_text_matches_the_brute_force_oracle() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "people",
            vec![
                ("position".into(), Kind::Point),
                ("born".into(), Kind::Int),
                ("body".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut rows = Vec::new();
    for i in 0..80u64 {
        let lon = (i as f64) * 0.02;
        let lat = ((i % 7) as f64) * 0.01;
        let born = 1980 + (i % 40) as i64;
        let body = if i % 5 == 0 { "flood river" } else { "road" };
        let id = db
            .put(
                collection,
                &format!("p{i:03}"),
                &json!({"position": point(lon, lat), "born": born, "body": body}),
            )
            .unwrap();
        rows.push((id, Point::new(lon, lat).unwrap(), born, body));
        if i % 16 == 15 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let position = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    let born = db
        .create_scalar_index(collection, "born", "born", false)
        .unwrap();
    let text = db.create_text_index(collection, "body", "body").unwrap();
    finish_build(&mut db, position);
    finish_build(&mut db, born);
    finish_build(&mut db, text);

    let center = Point::new(0.4, 0.0).unwrap();
    let brute = |keep: fn(i64, &str) -> bool| {
        let mut hits: Vec<(f64, EntityId)> = rows
            .iter()
            .filter(|(_, _, born, body)| keep(*born, *body))
            .map(|(id, point, _, _)| {
                (
                    sekejap_core::spatial_math::wgs84_distance_metres(center, *point),
                    *id,
                )
            })
            .collect();
        hits.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        hits
    };

    let born_filters = [QueryFilter::Scalar {
        index: born,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(1990)),
            upper: Bound::Excluded(ScalarValue::I64(2000)),
        },
    }];
    let born_oracle: Vec<EntityId> = brute(|year, _| (1990..2000).contains(&year))
        .into_iter()
        .take(10)
        .map(|(_, id)| id)
        .collect();
    let (ids, _, driver, primary_reads) = drain_distance(
        &db,
        collection,
        &born_filters,
        position,
        center,
        Some(10),
        4,
        CandidateDriver::Auto,
    );
    assert_eq!(driver, QueryDriver::Nearest { index: position });
    assert_eq!(primary_reads, 0);
    assert_eq!(ids, born_oracle);

    let text_filters = [QueryFilter::Text {
        index: text,
        query: "flood",
        matching: TextMatch::Any,
    }];
    let text_oracle: Vec<EntityId> = brute(|_, body| body.contains("flood"))
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    let (ids, _, driver, _) = drain_distance(
        &db,
        collection,
        &text_filters,
        position,
        center,
        None,
        8,
        CandidateDriver::Auto,
    );
    assert_eq!(driver, QueryDriver::Text(text));
    assert_eq!(ids, text_oracle);
}

/// (e) DESC is refused with a clear error, not silently treated as ascending.
#[test]
fn distance_order_refuses_descending() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("position".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.put(collection, "p0", &json!({"position": point(0.0, 0.0)}))
        .unwrap();
    db.commit().unwrap();
    let index = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    finish_build(&mut db, index);

    let err = db
        .prepare_query(QueryRequest {
            collection,
            filters: &[],
            order: QueryOrder::Distance {
                index,
                center: Point::new(0.0, 0.0).unwrap(),
                direction: SortDirection::Descending,
            },
            projection: Projection::Ids,
            total_limit: Some(1),
            driver: CandidateDriver::Auto,
        })
        .err()
        .expect("prepare must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.to_ascii_lowercase().contains("descend")
            || msg.to_ascii_lowercase().contains("ascending"),
        "descending distance must be refused clearly, got {msg}"
    );
}

/// (f) A deleted point never appears after delete, commit, checkpoint, reopen.
#[test]
fn distance_order_never_returns_a_deleted_point_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("position".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut ids = Vec::new();
    for n in 0..12 {
        ids.push(
            db.put(
                collection,
                &format!("p{n}"),
                &json!({"position": point(n as f64 * 0.01, 0.0)}),
            )
            .unwrap(),
        );
    }
    db.commit().unwrap();
    let index = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    finish_build(&mut db, index);
    let gone = ids[3];
    db.delete(collection, "p3").unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);

    let db = Database::open(&path, cfg()).unwrap();
    let center = Point::new(0.0, 0.0).unwrap();
    let (got, _, _, primary_reads) = drain_distance(
        &db,
        collection,
        &[],
        index,
        center,
        Some(12),
        5,
        CandidateDriver::Auto,
    );
    assert_eq!(primary_reads, 0);
    assert!(!got.contains(&gone), "deleted point {gone:?} in {got:?}");
    assert_eq!(got.len(), 11);
}


fn geom_json(g: &Geom) -> Value {
    match g {
        Geom::Point(x, y) => json!({"type": "Point", "coordinates": [x, y]}),
        Geom::LineString(c) => json!({"type": "LineString", "coordinates": c}),
        Geom::Polygon(rs) => json!({"type": "Polygon", "coordinates": rs}),
        Geom::MultiPoint(c) => json!({"type": "MultiPoint", "coordinates": c}),
        Geom::MultiLineString(rs) => json!({"type": "MultiLineString", "coordinates": rs}),
        Geom::MultiPolygon(ps) => json!({"type": "MultiPolygon", "coordinates": ps}),
    }
}

fn square(lon: f64, lat: f64, half: f64) -> Geom {
    Geom::Polygon(vec![vec![
        [lon - half, lat - half],
        [lon + half, lat - half],
        [lon + half, lat + half],
        [lon - half, lat + half],
        [lon - half, lat - half],
    ]])
}

fn hoop(lon: f64, lat: f64, outer: f64, inner: f64) -> Geom {
    Geom::Polygon(vec![
        vec![
            [lon - outer, lat - outer],
            [lon + outer, lat - outer],
            [lon + outer, lat + outer],
            [lon - outer, lat + outer],
            [lon - outer, lat - outer],
        ],
        vec![
            [lon - inner, lat - inner],
            [lon - inner, lat + inner],
            [lon + inner, lat + inner],
            [lon + inner, lat - inner],
            [lon - inner, lat - inner],
        ],
    ])
}

fn geometry_predicate(row: &Geom, predicate: &GeometryFilter) -> bool {
    match predicate {
        GeometryFilter::Intersects(q) => spatial_geometry::intersects(row, q),
        GeometryFilter::Within(q) => spatial_geometry::within(row, q),
        GeometryFilter::Contains(q) => spatial_geometry::contains(row, q),
        GeometryFilter::DWithin { geometry: q, metres } => {
            spatial_geometry::dwithin_m(row, q, *metres)
        }
    }
}

fn drain_geometry(
    db: &Database,
    collection: CollectionId,
    index: IndexId,
    predicate: GeometryFilter,
    order: QueryOrder<'_>,
    page: usize,
) -> Vec<EntityId> {
    let filter = QueryFilter::Geometry {
        index,
        predicate,
    };
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection,
            filters: &[filter],
            order,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut ids = Vec::new();
    loop {
        let result = prepared.next_page(page, QueryBudget::unlimited(), || false).unwrap();
        ids.extend(result.rows.iter().map(|row| row.id));
        if result.done || result.rows.is_empty() {
            break;
        }
    }
    ids
}

/// ~300 mixed geometries (points, lines, polygons with holes, multipolygons,
/// a dateline crosser, and shapes that land on all three ladder levels).
/// Every predicate against several query geometries returns exactly the
/// brute-force `spatial_geometry` answer, under EntityId and Driver order,
/// across page sizes 1/7/all with resume, and a deleted geometry is absent.
#[test]
fn geometry_filter_matches_a_spatial_geometry_brute_force_oracle() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let shapes = db
        .create_collection(
            "shapes",
            vec![("shape".into(), Kind::Geo), ("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();

    let mut rows: Vec<(EntityId, Geom)> = Vec::new();
    let mut push = |db: &mut Database, key: String, geom: Geom, born: i64| {
        let id = db
            .put(
                shapes,
                &key,
                &json!({"shape": geom_json(&geom), "born": born}),
            )
            .unwrap();
        rows.push((id, geom));
    };

    // Fine-level points and tiny squares.
    for i in 0..80u32 {
        let lon = -10.0 + f64::from(i % 20) * 0.05;
        let lat = -5.0 + f64::from(i / 20) * 0.05;
        push(&mut db, format!("pt{i:03}"), Geom::Point(lon, lat), 1990 + i as i64);
        push(
            &mut db,
            format!("sq{i:03}"),
            square(lon + 0.2, lat + 0.2, 0.01),
            2000 + i as i64,
        );
    }
    // Lines.
    for i in 0..20u32 {
        let x = 1.0 + f64::from(i) * 0.1;
        push(
            &mut db,
            format!("ln{i:03}"),
            Geom::LineString(vec![[x, 2.0], [x + 0.05, 2.1], [x + 0.1, 2.0]]),
            1980,
        );
    }
    // Polygons with holes (coarse-ish).
    for i in 0..20u32 {
        push(
            &mut db,
            format!("ho{i:03}"),
            hoop(20.0 + f64::from(i), 10.0, 0.4, 0.1),
            1970,
        );
    }
    // Medium squares that should drop to COARSE (a 1° box covers many fine cells).
    for i in 0..20u32 {
        push(
            &mut db,
            format!("md{i:03}"),
            square(40.0 + f64::from(i) * 2.0, 0.0, 0.6),
            1960,
        );
    }
    // Multipolygons.
    for i in 0..10u32 {
        let a = square(-30.0 + f64::from(i), 15.0, 0.05);
        let b = square(-29.0 + f64::from(i), 16.0, 0.05);
        let Geom::Polygon(ra) = a else { unreachable!() };
        let Geom::Polygon(rb) = b else { unreachable!() };
        push(
            &mut db,
            format!("mp{i:03}"),
            Geom::MultiPolygon(vec![ra, rb]),
            1950,
        );
    }
    // World-bucket: a wide continent-scale polygon and a dateline crosser.
    push(
        &mut db,
        "world".into(),
        square(0.0, 0.0, 40.0),
        1940,
    );
    push(
        &mut db,
        "date".into(),
        Geom::Polygon(vec![vec![
            [170.0, 1.0],
            [-170.0, 1.0],
            [-170.0, 3.0],
            [170.0, 3.0],
            [170.0, 1.0],
        ]]),
        1930,
    );
    // One extra that will be deleted after the index is built.
    let deleted = db
        .put(
            shapes,
            "gone",
            &json!({"shape": geom_json(&square(8.0, 8.0, 0.02)), "born": 1900}),
        )
        .unwrap();
    db.commit().unwrap();

    let index = db.create_geometry_index(shapes, "by_shape", "shape").unwrap();
    db.build_index_to_ready(index, 32).unwrap();
    db.commit().unwrap();
    assert!(db.delete(shapes, "gone").unwrap());
    db.commit().unwrap();
    rows.retain(|(id, _)| *id != deleted);
    assert_eq!(rows.len(), 80 * 2 + 20 + 20 + 20 + 10 + 2);

    let queries: Vec<GeometryFilter> = vec![
        GeometryFilter::Intersects(square(0.0, 0.0, 2.0)),
        GeometryFilter::Within(square(0.0, 0.0, 12.0)),
        GeometryFilter::Contains(Geom::Point(0.05, 0.05)),
        GeometryFilter::DWithin {
            geometry: Geom::Point(1.0, 2.05),
            metres: 50_000.0,
        },
        GeometryFilter::Intersects(square(20.0, 10.0, 0.5)),
        GeometryFilter::Intersects(Geom::Point(170.5, 2.0)),
        GeometryFilter::Within(square(40.0, 0.0, 8.0)),
        GeometryFilter::Contains(square(-10.0, -5.0, 0.001)),
    ];

    for predicate in &queries {
        let mut expected: Vec<EntityId> = rows
            .iter()
            .filter(|(_, g)| geometry_predicate(g, predicate))
            .map(|(id, _)| *id)
            .collect();
        expected.sort();
        for order in [QueryOrder::EntityId, QueryOrder::Driver] {
            for page in [1usize, 7, 8192] {
                let mut got = drain_geometry(&db, shapes, index, predicate.clone(), order, page);
                if matches!(order, QueryOrder::EntityId) {
                    // Driver order is (level, cell, seq); EntityId is id order.
                    got.sort();
                    assert_eq!(
                        got, expected,
                        "mismatch order={order:?} page={page} predicate={predicate:?}"
                    );
                } else {
                    let mut sorted = got.clone();
                    sorted.sort();
                    assert_eq!(
                        sorted, expected,
                        "driver-order set mismatch page={page} predicate={predicate:?}"
                    );
                    // Resume must not duplicate.
                    let unique = got.len();
                    got.sort();
                    got.dedup();
                    assert_eq!(got.len(), unique, "driver-order page={page} duplicated an entity");
                }
            }
        }
    }
}

/// A geometry filter combined with a born range (C1 membership set) and with
/// a text driver (geometry as a non-driving predicate).
#[test]
fn geometry_filter_combines_with_a_born_range_and_a_text_driver() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let rows_c = db
        .create_collection(
            "combo",
            vec![
                ("shape".into(), Kind::Geo),
                ("born".into(), Kind::Int),
                ("tag".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    let mut stored = Vec::new();
    for i in 0..80u32 {
        let geom = square(f64::from(i) * 0.02, 0.0, 0.005);
        let born = 1990 + (i as i64 % 20);
        let tag = if i % 3 == 0 { "alpha" } else { "beta" };
        let id = db
            .put(
                rows_c,
                &format!("r{i:03}"),
                &json!({"shape": geom_json(&geom), "born": born, "tag": tag}),
            )
            .unwrap();
        stored.push((id, geom, born, tag));
    }
    db.commit().unwrap();
    let geo = db.create_geometry_index(rows_c, "by_shape", "shape").unwrap();
    db.build_index_to_ready(geo, 16).unwrap();
    let born = db.create_scalar_index(rows_c, "by_born", "born", false).unwrap();
    db.build_index_to_ready(born, 16).unwrap();
    let text = db.create_text_index(rows_c, "by_tag", "tag").unwrap();
    db.build_index_to_ready(text, 16).unwrap();
    db.commit().unwrap();

    let window = square(0.4, 0.0, 0.3);
    let geo_filter = QueryFilter::Geometry {
        index: geo,
        predicate: GeometryFilter::Intersects(window.clone()),
    };
    let born_filter = QueryFilter::Scalar {
        index: born,
        predicate: ScalarFilter::Range {
            lower: std::ops::Bound::Included(ScalarValue::I64(1995)),
            upper: std::ops::Bound::Excluded(ScalarValue::I64(2005)),
        },
    };
    let mut expected: Vec<EntityId> = stored
        .iter()
        .filter(|(_, g, b, _)| {
            spatial_geometry::intersects(g, &window) && *b >= 1995 && *b < 2005
        })
        .map(|(id, _, _, _)| *id)
        .collect();
    expected.sort();

    let mut prepared = db
        .prepare_query(QueryRequest {
            collection: rows_c,
            filters: &[geo_filter, born_filter],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let page = prepared
        .next_page(8192, QueryBudget::unlimited(), || false)
        .unwrap();
    let mut got: Vec<_> = page.rows.iter().map(|r| r.id).collect();
    got.sort();
    assert_eq!(got, expected);
    // C1: the born range is a membership set, so it must not add a primary
    // read on top of the geometry refine.
    assert!(
        page.work.primary_reads <= page.work.candidates,
        "born range added extra rows: {:?}",
        page.work
    );

    let text_filter = QueryFilter::Text {
        index: text,
        query: "alpha",
        matching: TextMatch::Any,
    };
    let geo_nd = QueryFilter::Geometry {
        index: geo,
        predicate: GeometryFilter::Intersects(window.clone()),
    };
    let mut expected_text: Vec<EntityId> = stored
        .iter()
        .filter(|(_, g, _, tag)| *tag == "alpha" && spatial_geometry::intersects(g, &window))
        .map(|(id, _, _, _)| *id)
        .collect();
    expected_text.sort();
    let mut prepared = db
        .prepare_query(QueryRequest {
            collection: rows_c,
            filters: &[text_filter, geo_nd],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Filter(0),
        })
        .unwrap();
    let page = prepared
        .next_page(8192, QueryBudget::unlimited(), || false)
        .unwrap();
    assert_eq!(page.driver, QueryDriver::Text(text));
    let mut got: Vec<_> = page.rows.iter().map(|r| r.id).collect();
    got.sort();
    assert_eq!(got, expected_text);
}

/// Unsupported shapes are refused with a clear error: a Geo field with no
/// geometry index, and a Point index given a Geometry filter.
#[test]
fn geometry_filter_refuses_a_missing_index_and_a_point_index() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let rows = db
        .create_collection(
            "g",
            vec![
                ("shape".into(), Kind::Geo),
                ("pt".into(), Kind::Point),
                ("n".into(), Kind::Int),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    db.put(
        rows,
        "a",
        &json!({
            "shape": geom_json(&square(0.0, 0.0, 0.1)),
            "pt": {"type": "Point", "coordinates": [0.0, 0.0]},
            "n": 1,
        }),
    )
    .unwrap();
    db.commit().unwrap();
    let scalar = db.create_scalar_index(rows, "by_n", "n", false).unwrap();
    db.build_index_to_ready(scalar, 8).unwrap();
    let point = db.create_point_index(rows, "by_pt", "pt").unwrap();
    db.build_index_to_ready(point, 8).unwrap();
    db.commit().unwrap();

    let query = Geom::Point(0.0, 0.0);
    let err = db
        .prepare_query(QueryRequest {
            collection: rows,
            filters: &[QueryFilter::Geometry {
                index: scalar,
                predicate: GeometryFilter::Intersects(query.clone()),
            }],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .err()
        .expect("prepare must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("spatial geometry") || msg.contains("Geometry"),
        "geo field without a geometry index must be refused clearly: {msg}"
    );

    let err = db
        .prepare_query(QueryRequest {
            collection: rows,
            filters: &[QueryFilter::Geometry {
                index: point,
                predicate: GeometryFilter::Intersects(query),
            }],
            order: QueryOrder::EntityId,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .err()
        .expect("prepare must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("not a point index") || msg.contains("spatial geometry"),
        "a Point index given a Geometry filter must be refused clearly: {msg}"
    );
}
