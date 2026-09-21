//! WHAT A BATCH OF RELATIONSHIPS COSTS, and what it leaves behind.
//!
//! `Database::link_many` writes many edges of one context and one type under
//! one validation pass and in key order. These tests pin the two things that
//! makes it: the edge set it produces is EXACTLY the set a brute-force
//! `BTreeSet` in the test process says it should be, and the work it charges
//! is strictly less than the same edges written one `put_edge` at a time.
//!
//! The oracle is held here, never read back out of the database: the test
//! decides what the graph must be, the engine is asked what it is, and the
//! two are compared. `verify_indexed_source` then says the file itself is
//! clean -- primary postings and reverse markers agree, both directions of
//! every edge are there.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        CollectionId, CollectionOptions, Database, Direction, EdgeKey, EdgeTypeId, EntityId, Error,
        GraphContextId, NeighborRequest, NewEdge,
    },
    Kind,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// `rows` rows in one collection, committed and checkpointed, plus one edge
/// type. The load is a separate, earlier pass, which is what the 50K battery's
/// `--graph` stage does and what decides how many endpoints are still on the
/// handle's fresh-identity map when the edges arrive.
fn population(rows: u64) -> (tempfile::TempDir, Database, CollectionId, EdgeTypeId) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("name".into(), Kind::Text)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    db.commit().unwrap();
    for i in 0..rows {
        db.put(people, &format!("p{i:08}"), &json!({"name": "x"}))
            .unwrap();
        if (i + 1) % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    (dir, db, people, knows)
}

fn entity(people: CollectionId, i: u64) -> EntityId {
    EntityId {
        collection: people,
        sequence: i,
    }
}

/// The 50K battery's own edge shape, scaled: every row points at three others
/// picked so that the DESTINATIONS of one batch are spread across the whole
/// keyspace, which is what the `related` graph's "three nearest by location"
/// does to a corpus whose row order is not its spatial order.
fn battery_shape(people: CollectionId, rows: u64) -> Vec<NewEdge> {
    let mut edges = Vec::with_capacity(rows as usize * 3);
    for i in 0..rows {
        for step in [7_919u64, 104_729, 15_485_863] {
            let destination = (i.wrapping_mul(step) % rows) + 1;
            if destination == i + 1 {
                continue;
            }
            edges.push(NewEdge {
                source: entity(people, i + 1),
                destination: entity(people, destination),
                properties: json!({"weight": (i % 100) as f64 / 100.0, "since": 19_500_101 + i as i64}),
            });
        }
    }
    edges
}

/// Every edge the engine holds for this type, read back one source at a time
/// through the public neighbour call: `(source, destination) -> properties`.
fn edges_on_disk(
    db: &Database,
    people: CollectionId,
    knows: EdgeTypeId,
    rows: u64,
) -> BTreeMap<(u64, u64), serde_json::Value> {
    let mut found = BTreeMap::new();
    for i in 1..=rows {
        for edge in db
            .neighbors(NeighborRequest {
                entity: entity(people, i),
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(knows),
                limit: 256,
            })
            .unwrap()
        {
            found.insert(
                (edge.key.source.sequence, edge.key.destination.sequence),
                edge.properties,
            );
        }
    }
    found
}

/// THE ORACLE. The test decides what the graph is; the engine is asked what it
/// is; the two must be the same map, key for key and property for property.
#[test]
fn a_batch_write_leaves_exactly_the_edge_set_the_test_computed_for_itself() {
    const ROWS: u64 = 4_000;
    let (_dir, mut db, people, knows) = population(ROWS);
    let batch = battery_shape(people, ROWS);

    // The brute-force answer, built in the test process from the same rule.
    let mut oracle: BTreeMap<(u64, u64), serde_json::Value> = BTreeMap::new();
    for edge in &batch {
        oracle.insert(
            (edge.source.sequence, edge.destination.sequence),
            edge.properties.clone(),
        );
    }

    for chunk in batch.chunks(256) {
        db.link_many(GraphContextId::BASE, knows, chunk).unwrap();
        db.commit().unwrap();
    }
    db.commit().unwrap();

    let found = edges_on_disk(&db, people, knows, ROWS);
    assert_eq!(
        found.len(),
        oracle.len(),
        "the batch wrote {} distinct edges; the test computed {}",
        found.len(),
        oracle.len()
    );
    assert_eq!(found, oracle, "the edge set on disk is not the one the test computed");
    assert_eq!(db.scan_count_edges().unwrap(), oracle.len() as u64);
}

/// L3. The reverse mirror is not an afterthought of the batch: every edge the
/// batch wrote is reachable from its DESTINATION as well as from its source,
/// and the file the verifier reads says so with no issue of its own.
#[test]
fn both_directions_of_every_edge_in_a_batch_are_there_after_the_commit() {
    const ROWS: u64 = 1_000;
    let (dir, mut db, people, knows) = population(ROWS);
    let batch = battery_shape(people, ROWS);
    let mut incoming: BTreeSet<(u64, u64)> = BTreeSet::new();
    for edge in &batch {
        incoming.insert((edge.destination.sequence, edge.source.sequence));
    }
    for chunk in batch.chunks(256) {
        db.link_many(GraphContextId::BASE, knows, chunk).unwrap();
        db.commit().unwrap();
    }
    db.commit().unwrap();

    let mut found: BTreeSet<(u64, u64)> = BTreeSet::new();
    for i in 1..=ROWS {
        for edge in db
            .neighbors(NeighborRequest {
                entity: entity(people, i),
                direction: Direction::Incoming,
                context: GraphContextId::BASE,
                edge_type: Some(knows),
                limit: 256,
            })
            .unwrap()
        {
            found.insert((i, edge.key.source.sequence));
        }
    }
    assert_eq!(found, incoming, "the reverse mirror is not the transpose of the batch");

    let path = dir.path().join("db");
    drop(db);
    let mut issues = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        issues.push(issue.message.clone())
    })
    .unwrap();
    assert!(
        report.complete && report.clean,
        "a batch-written graph did not verify clean: {issues:?}"
    );
}

/// Page accesses for `count` edges written one `put_edge` at a time.
fn pages_one_at_a_time(db: &mut Database, knows: EdgeTypeId, batch: &[NewEdge]) -> f64 {
    let before = db.pool_accesses().unwrap();
    for edge in batch {
        db.put_edge(
            GraphContextId::BASE,
            edge.source,
            knows,
            edge.destination,
            &edge.properties,
        )
        .unwrap();
    }
    db.commit().unwrap();
    (db.pool_accesses().unwrap() - before) as f64 / batch.len() as f64
}

/// The same edges through `link_many`, at the same commit cadence.
fn pages_in_batches(db: &mut Database, knows: EdgeTypeId, batch: &[NewEdge]) -> f64 {
    let before = db.pool_accesses().unwrap();
    for chunk in batch.chunks(256) {
        db.link_many(GraphContextId::BASE, knows, chunk).unwrap();
        db.commit().unwrap();
    }
    db.commit().unwrap();
    (db.pool_accesses().unwrap() - before) as f64 / batch.len() as f64
}

/// L2, on the handle that LOADED the rows: the endpoint reads and the pair
/// probe are already free there, so what a batch saves is the keyspace
/// locality -- two sorted runs instead of one alternation per edge.
#[test]
fn a_batch_touches_fewer_pages_than_the_same_edges_one_at_a_time() {
    const ROWS: u64 = 8_000;
    let (_dir_a, mut db_a, people_a, knows_a) = population(ROWS);
    let one_at_a_time = pages_one_at_a_time(&mut db_a, knows_a, &battery_shape(people_a, ROWS));
    let (_dir_b, mut db_b, people_b, knows_b) = population(ROWS);
    let batched = pages_in_batches(&mut db_b, knows_b, &battery_shape(people_b, ROWS));
    assert!(
        batched < one_at_a_time,
        "one edge at a time took {one_at_a_time:.2} page accesses per edge and the batch \
         took {batched:.2}; a batch that reorders nothing would be the same number"
    );
}

/// L2, on a REOPENED handle -- the shape a `--reuse --graph` battery run has,
/// where no endpoint is on the fresh-identity map. Endpoint existence is a
/// property of an ENTITY, so a batch of 256 edges over ~90 distinct sources
/// proves each source once instead of once per edge.
#[test]
fn a_batch_proves_each_endpoint_once_rather_than_once_per_edge() {
    const ROWS: u64 = 8_000;
    let make = |edges: &dyn Fn(&mut Database, EdgeTypeId, &[NewEdge]) -> f64| -> f64 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let (people, knows) = {
            let mut db = Database::create(&path, cfg()).unwrap();
            let people = db
                .create_collection(
                    "people",
                    vec![("name".into(), Kind::Text)],
                    CollectionOptions::default(),
                )
                .unwrap();
            db.enable_graph().unwrap();
            let knows = db.create_edge_type("knows").unwrap();
            db.commit().unwrap();
            for i in 0..ROWS {
                db.put(people, &format!("p{i:08}"), &json!({"name": "x"}))
                    .unwrap();
                if (i + 1) % 256 == 0 {
                    db.commit().unwrap();
                }
            }
            db.commit().unwrap();
            db.checkpoint().unwrap();
            (people, knows)
        };
        // A fresh handle: it allocated nothing, so every endpoint costs a real
        // descent unless the caller stops asking for the same one twice.
        let mut db = Database::open(&path, cfg()).unwrap();
        let shape = battery_shape(people, ROWS);
        edges(&mut db, knows, &shape)
    };
    let one_at_a_time = make(&|db, knows, batch| pages_one_at_a_time(db, knows, batch));
    let batched = make(&|db, knows, batch| pages_in_batches(db, knows, batch));
    assert!(
        batched < one_at_a_time * 0.9,
        "on a reopened handle one edge at a time took {one_at_a_time:.2} page accesses per \
         edge and the batch took {batched:.2}; the batch proves ~90 distinct sources per 256 \
         edges instead of 256, so it must be well under"
    );
}

/// A batch is all or nothing on its own validation: an endpoint that is not
/// there refuses the whole call, and nothing of it is in the transaction.
#[test]
fn a_batch_naming_a_row_that_is_not_there_is_refused_before_anything_is_written() {
    const ROWS: u64 = 64;
    let (_dir, mut db, people, knows) = population(ROWS);
    let batch = vec![
        NewEdge {
            source: entity(people, 1),
            destination: entity(people, 2),
            properties: json!({}),
        },
        NewEdge {
            source: entity(people, 3),
            // Never allocated.
            destination: entity(people, ROWS + 500),
            properties: json!({}),
        },
    ];
    assert!(matches!(
        db.link_many(GraphContextId::BASE, knows, &batch),
        Err(Error::NotFound(_))
    ));
    db.rollback().unwrap();
    assert_eq!(db.scan_count_edges().unwrap(), 0);
}

/// Inside one batch a repeated quadruple is last-wins, which is the rule two
/// consecutive `put_edge` calls of the same quadruple already follow. The sort
/// that gives the batch its key order is stable so that this stays true.
#[test]
fn a_quadruple_repeated_inside_one_batch_keeps_the_last_properties() {
    const ROWS: u64 = 64;
    let (_dir, mut db, people, knows) = population(ROWS);
    let batch = vec![
        NewEdge {
            source: entity(people, 1),
            destination: entity(people, 2),
            properties: json!({"weight": 0.1}),
        },
        NewEdge {
            source: entity(people, 1),
            destination: entity(people, 3),
            properties: json!({"weight": 0.9}),
        },
        NewEdge {
            source: entity(people, 1),
            destination: entity(people, 2),
            properties: json!({"weight": 0.5}),
        },
    ];
    let keys = db.link_many(GraphContextId::BASE, knows, &batch).unwrap();
    db.commit().unwrap();
    assert_eq!(keys.len(), 3);
    assert_eq!(
        keys[0],
        EdgeKey {
            source: entity(people, 1),
            context: GraphContextId::BASE,
            edge_type: knows,
            destination: entity(people, 2),
        }
    );
    let found = edges_on_disk(&db, people, knows, ROWS);
    assert_eq!(found.len(), 2);
    assert_eq!(found[&(1, 2)], json!({"weight": 0.5}));
    assert_eq!(found[&(1, 3)], json!({"weight": 0.9}));
}
