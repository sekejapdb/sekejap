//! Phase 2 point-index contract. Expected membership and distance values are
//! independent constants or direct coordinate predicates, not engine helpers.

use e4_prototype::{
    collections::{
        CollectionOptions, Database, EntityId, Error, IndexFamily, IndexState, SpatialCandidates,
    },
    spatial_math::{Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn point(lon: f64, lat: f64) -> Value {
    json!({"type":"Point","coordinates":[lon,lat]})
}

fn p(lon: f64, lat: f64) -> Point {
    Point::new(lon, lat).unwrap()
}

fn finish_build(db: &mut Database, index: e4_prototype::collections::IndexId, batch: usize) {
    while !db.build_index_step(index, batch).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
}

fn bbox(
    db: &Database,
    index: e4_prototype::collections::IndexId,
    bounds: Bounds,
    limit: usize,
    candidates: SpatialCandidates<'_>,
    max_examined: usize,
) -> e4_prototype::collections::Result<Vec<EntityId>> {
    db.query_point_bbox(index, bounds, limit, candidates, max_examined, || false)
}

fn radius(
    db: &Database,
    index: e4_prototype::collections::IndexId,
    center: Point,
    metres: f64,
    limit: usize,
    candidates: SpatialCandidates<'_>,
    max_examined: usize,
) -> e4_prototype::collections::Result<Vec<EntityId>> {
    db.query_point_radius(
        index,
        center,
        metres,
        limit,
        candidates,
        max_examined,
        || false,
    )
}

#[test]
fn bbox_radius_nearest_two_fields_and_collections_match_oracles() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![
                ("position".into(), Kind::Point),
                ("home".into(), Kind::Point),
                ("label".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let places = db
        .create_collection(
            "places",
            vec![("position".into(), Kind::Point)],
            CollectionOptions::default(),
        )
        .unwrap();

    let a = db
        .put(
            people,
            "a",
            &json!({"position":point(0.0,0.0),"home":point(50.0,50.0),"label":"origin"}),
        )
        .unwrap();
    let b = db
        .put(
            people,
            "b",
            &json!({"position":point(1.0,0.0),"home":point(0.0,0.0),"label":"east"}),
        )
        .unwrap();
    let c = db
        .put(
            people,
            "c",
            &json!({"position":point(2.0,0.0),"home":point(-50.0,-50.0),"label":"farther"}),
        )
        .unwrap();
    let east_dateline = db
        .put(
            people,
            "east-dateline",
            &json!({"position":point(180.0,5.0),"home":point(10.0,10.0),"label":"edge"}),
        )
        .unwrap();
    let west_dateline = db
        .put(
            people,
            "west-dateline",
            &json!({"position":point(-179.5,-5.0),"home":point(20.0,20.0),"label":"edge"}),
        )
        .unwrap();
    let missing = db
        .put(people, "missing", &json!({"label":"missing"}))
        .unwrap();
    let null = db
        .put(
            people,
            "null",
            &json!({"position":Value::Null,"home":Value::Null,"label":"null"}),
        )
        .unwrap();
    let other = db
        .put(places, "other-origin", &json!({"position":point(0.0,0.0)}))
        .unwrap();
    db.commit().unwrap();

    let position = db
        .create_point_index(people, "position", "position")
        .unwrap();
    let home = db.create_point_index(people, "home", "home").unwrap();
    let other_position = db
        .create_point_index(places, "position", "position")
        .unwrap();
    assert_eq!(
        db.index_info(position).unwrap().family,
        IndexFamily::SpatialPoint
    );
    assert!(matches!(
        db.index_info(position).unwrap().state,
        IndexState::Building { .. }
    ));
    finish_build(&mut db, position, 2);
    finish_build(&mut db, home, 2);
    finish_build(&mut db, other_position, 2);

    assert_eq!(
        bbox(
            &db,
            position,
            Bounds::new(170.0, -170.0, -10.0, 10.0).unwrap(),
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![east_dateline, west_dateline]
    );
    // A non-wrapping rectangle touching -180 also contains stored +180.
    assert_eq!(
        bbox(
            &db,
            position,
            Bounds::new(-180.0, -179.0, -10.0, 10.0).unwrap(),
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![east_dateline, west_dateline]
    );
    assert_eq!(
        bbox(
            &db,
            home,
            Bounds::new(-0.1, 0.1, -0.1, 0.1).unwrap(),
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![b]
    );
    assert_eq!(
        bbox(
            &db,
            other_position,
            Bounds::new(-0.1, 0.1, -0.1, 0.1).unwrap(),
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![other]
    );

    // GeographicLib WGS84 equatorial distance for one degree is
    // 111319.49079327357 m. Radius acceptance is inclusive.
    assert_eq!(
        radius(
            &db,
            position,
            p(0.0, 0.0),
            111_319.490_794,
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![a, b]
    );
    let absent = [missing, null];
    assert!(db
        .query_point_nearest(
            position,
            p(0.0, 0.0),
            2,
            SpatialCandidates::SortedUnique(&absent),
            absent.len(),
            || false,
        )
        .unwrap()
        .is_empty());
    assert_eq!(
        radius(
            &db,
            position,
            p(0.0, 0.0),
            111_319.0,
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![a]
    );

    let nearest = db
        .query_point_nearest(position, p(0.0, 0.0), 3, SpatialCandidates::All, 5, || {
            false
        })
        .unwrap();
    assert_eq!(
        nearest.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![a, b, c]
    );
    assert!((nearest[1].distance_metres - 111_319.490_793_273_57).abs() < 1e-6);

    // Filtering precedes top-k: the global winner is not in this set.
    let filtered = [b, c, west_dateline];
    let nearest = db
        .query_point_nearest(
            position,
            p(0.0, 0.0),
            2,
            SpatialCandidates::SortedUnique(&filtered),
            filtered.len(),
            || false,
        )
        .unwrap();
    assert_eq!(
        nearest.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![b, c]
    );

    // Bbox/radius limits choose the smallest matching stable identities, not
    // Hilbert traversal order.
    assert_eq!(
        bbox(
            &db,
            position,
            Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap(),
            2,
            SpatialCandidates::All,
            5,
        )
        .unwrap(),
        vec![a, b]
    );
}

#[test]
fn build_live_crud_snapshot_rollback_drop_and_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![
                ("position".into(), Kind::Point),
                ("name".into(), Kind::Text),
            ],
            Default::default(),
        )
        .unwrap();
    let old = db
        .put(
            collection,
            "old",
            &json!({"position":point(0.0,0.0),"name":"old"}),
        )
        .unwrap();
    db.commit().unwrap();
    let index = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    // READY and BUILDING indexes share live maintenance. This row is written
    // after index declaration and before the historical build advances.
    let during_build = db
        .put(
            collection,
            "during-build",
            &json!({"position":point(50.0,50.0),"name":"during-build"}),
        )
        .unwrap();
    db.commit().unwrap();
    finish_build(&mut db, index, 1);
    assert_eq!(
        bbox(
            &db,
            index,
            Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap(),
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![old, during_build]
    );
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();

    let new = db
        .put(
            collection,
            "new",
            &json!({"position":point(1.0,0.0),"name":"new"}),
        )
        .unwrap();
    db.update(collection, "old", &json!({"position":point(10.0,10.0)}))
        .unwrap();
    db.commit().unwrap();
    assert_eq!(
        radius(
            &db,
            index,
            p(0.0, 0.0),
            120_000.0,
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![new]
    );
    assert_eq!(
        radius(
            &snapshot,
            index,
            p(0.0, 0.0),
            120_000.0,
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![old]
    );

    db.update(collection, "new", &json!({"position":Value::Null}))
        .unwrap();
    assert!(radius(
        &db,
        index,
        p(0.0, 0.0),
        120_000.0,
        10,
        SpatialCandidates::All,
        10,
    )
    .unwrap()
    .is_empty());
    db.rollback().unwrap();
    assert_eq!(
        radius(
            &db,
            index,
            p(0.0, 0.0),
            120_000.0,
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![new]
    );

    assert!(db.delete(collection, "new").unwrap());
    db.commit().unwrap();
    let filtered = [new];
    assert!(db
        .query_point_nearest(
            index,
            p(0.0, 0.0),
            1,
            SpatialCandidates::SortedUnique(&filtered),
            1,
            || false,
        )
        .unwrap()
        .is_empty());
    drop(snapshot);
    drop(db);

    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        bbox(
            &db,
            index,
            Bounds::new(9.0, 11.0, 9.0, 11.0).unwrap(),
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![old]
    );
    let snapshot = Database::open_snapshot(&path, cfg()).unwrap();
    db.begin_drop_index(index).unwrap();
    assert!(bbox(
        &db,
        index,
        Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap(),
        10,
        SpatialCandidates::All,
        10,
    )
    .is_err());
    assert_eq!(
        bbox(
            &snapshot,
            index,
            Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap(),
            10,
            SpatialCandidates::All,
            10,
        )
        .unwrap(),
        vec![old, during_build]
    );
    while !db.drop_index_step(index, 1).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert!(db.index_info(index).is_err());
}

#[test]
fn validation_work_bounds_cancellation_and_candidate_order_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Database::create(temp.path().join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![
                ("position".into(), Kind::Point),
                ("name".into(), Kind::Text),
            ],
            Default::default(),
        )
        .unwrap();
    assert!(db.create_point_index(collection, "bad", "name").is_err());
    assert!(db
        .put(
            collection,
            "invalid",
            &json!({"position":point(181.0,0.0),"name":"invalid"}),
        )
        .is_err());
    let mut ids = Vec::new();
    for n in 0..4 {
        ids.push(
            db.put(
                collection,
                &format!("p{n}"),
                &json!({"position":point(40.0+n as f64,30.0),"name":format!("p{n}")}),
            )
            .unwrap(),
        );
    }
    let other_collection = db
        .create_collection(
            "other",
            vec![("position".into(), Kind::Point)],
            Default::default(),
        )
        .unwrap();
    let other = db
        .put(
            other_collection,
            "other",
            &json!({"position":point(0.0,0.0)}),
        )
        .unwrap();
    db.commit().unwrap();
    let index = db
        .create_point_index(collection, "position", "position")
        .unwrap();
    assert!(bbox(
        &db,
        index,
        Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap(),
        1,
        SpatialCandidates::All,
        4,
    )
    .is_err());
    finish_build(&mut db, index, 2);

    let world = Bounds::new(-180.0, 180.0, -90.0, 90.0).unwrap();
    assert!(matches!(
        bbox(&db, index, world, 1, SpatialCandidates::All, 3,),
        Err(Error::Kernel(kernel::Error::ResourceLimit(_)))
    ));
    assert!(matches!(
        db.query_point_nearest(index, p(0.0, 0.0), 1, SpatialCandidates::All, 3, || false,),
        Err(Error::Kernel(kernel::Error::ResourceLimit(_)))
    ));
    assert!(bbox(&db, index, world, 65_537, SpatialCandidates::All, 4,).is_err());
    assert!(radius(&db, index, p(0.0, 0.0), -1.0, 1, SpatialCandidates::All, 4,).is_err());

    let duplicate = [ids[0], ids[0]];
    assert!(bbox(
        &db,
        index,
        world,
        1,
        SpatialCandidates::SortedUnique(&duplicate),
        2,
    )
    .is_err());
    let unsorted = [ids[1], ids[0]];
    assert!(bbox(
        &db,
        index,
        world,
        1,
        SpatialCandidates::SortedUnique(&unsorted),
        2,
    )
    .is_err());
    let wrong_collection = [other];
    assert!(bbox(
        &db,
        index,
        world,
        1,
        SpatialCandidates::SortedUnique(&wrong_collection),
        1,
    )
    .is_err());

    assert!(matches!(
        db.query_point_bbox(index, world, 4, SpatialCandidates::All, 4, || true,),
        Err(Error::Cancelled)
    ));
    let mut polls = 0;
    assert!(matches!(
        db.query_point_nearest(index, p(0.0, 0.0), 4, SpatialCandidates::All, 4, || {
            polls += 1;
            polls == 3
        },),
        Err(Error::Cancelled)
    ));
    // Cancellation and read budget errors do not poison the handle.
    assert_eq!(
        bbox(&db, index, world, 4, SpatialCandidates::All, 4,).unwrap(),
        ids
    );
}
