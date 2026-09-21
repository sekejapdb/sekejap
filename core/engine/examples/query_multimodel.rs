//! Run with a new database path:
//! `cargo run --example query_multimodel -- /tmp/e4-query-example`
use sekejap_core::{
    Kind,
    collections::{
        BfsRequest, CandidateDriver, CollectionOptions, Database, Direction, GraphContextId,
        PointFilter, Projection, QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter,
        ScalarValue, TextMatch, VectorMetric,
    },
    spatial_math::Bounds,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: query_multimodel NEW_DATABASE_PATH")?;
    let mut db = Database::create(
        path,
        Config {
            budget_bytes: 8 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    db.enable_graph()?;
    let people = db.create_collection(
        "people",
        vec![
            ("active".into(), Kind::Bool),
            ("body".into(), Kind::Text),
            ("embedding".into(), Kind::Vector(2)),
            ("position".into(), Kind::Point),
        ],
        CollectionOptions::default(),
    )?;
    let p0 = db.put(
        people,
        "p0",
        &json!({
            "active":true,"body":"flood flood river","embedding":[1.0,0.0],
            "position":{"type":"Point","coordinates":[0.0,0.0]}
        }),
    )?;
    let p1 = db.put(
        people,
        "p1",
        &json!({
            "active":true,"body":"flood road","embedding":[1.0,1.0],
            "position":{"type":"Point","coordinates":[1.0,0.0]}
        }),
    )?;
    let edge = db.link(p0, "knows", p1, "", &json!({}))?;
    db.commit()?;

    let active = db.create_scalar_index(people, "active", "active", false)?;
    let position = db.create_point_index(people, "position", "position")?;
    let text = db.create_text_index(people, "body", "body")?;
    let vector = db.create_quantized_vector_index(people, "embedding_int8", "embedding")?;
    db.commit()?;
    for index in [active, position, text, vector] {
        while !db.build_index_step(index, 32)? {
            db.commit()?;
        }
        db.commit()?;
    }

    let filters = [
        QueryFilter::Graph(BfsRequest {
            seed: p0,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(edge.edge_type),
            min_depth: 1,
            max_depth: 2,
            include_seed: false,
            max_visited: 1_000,
            max_edges: 10_000,
            result_limit: 1_000,
            edge_where: &[],
            node_where: &[],
        }),
        QueryFilter::Scalar {
            index: active,
            predicate: ScalarFilter::Eq(ScalarValue::Bool(true)),
        },
        QueryFilter::Point {
            index: position,
            predicate: PointFilter::Bbox(Bounds::new(0.0, 1.0, 0.0, 1.0)?),
        },
        QueryFilter::Text {
            index: text,
            query: "flood",
            matching: TextMatch::Any,
        },
    ];
    let projection = ["body", "embedding"];
    let mut query = db.prepare_query(QueryRequest {
        collection: people,
        filters: &filters,
        order: QueryOrder::ApproximateVector {
            index: vector,
            query: &[1.0, 0.0],
            metric: VectorMetric::Cosine,
            ef: 32,
        },
        projection: Projection::Fields(&projection),
        total_limit: Some(10),
        driver: CandidateDriver::Auto,
    })?;
    let page = query.next_page(10, QueryBudget::unlimited(), || false)?;
    println!(
        "mode=approximate_symmetric_int8_scan_with_exact_rerank driver={:?} approximation={:?} work={:?}",
        page.driver, page.approximation, page.work
    );
    for row in page.rows {
        println!(
            "id={:?} rank={:?} fields={:?}",
            row.id, row.order, row.projected
        );
    }
    Ok(())
}
