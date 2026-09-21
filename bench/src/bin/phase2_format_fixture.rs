//! Candidate Phase-2 scalar+graph compatibility fixture helper.
//!
//! This is not a released/frozen-format claim and never replaces the Phase-1
//! corpus. `generate` requires a new directory. `upgrade` requires an explicit
//! external source manifest and a separately copied target.
use sekejap_core::{
    collections::{
        BfsRequest, CollectionId, CollectionOptions, Database, Direction, EdgeTypeId, EntityId,
        GraphContextId, IndexId, IndexState, NeighborRequest, ScalarPredicate,
    },
    pagewal::{create_compact_cells, PageWalStore},
    Kind,
};
use kernel::{
    io::IoMode,
    page::{PageRef, PAGE_SIZE},
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

const CACHE_BYTES: usize = 1 << 20;
const MANIFEST: &str = "PHASE2_FIXTURE.json";
const FORMAT: &str = "e4-phase2-scalar-graph-candidate-v1";
const PHYSICAL_FEATURES_OFFSET: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Original,
    Updated,
    Roundtrip,
}

fn cfg() -> Config {
    Config {
        budget_bytes: CACHE_BYTES,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn entity(collection: u32, sequence: u64) -> EntityId {
    EntityId {
        collection: CollectionId(collection),
        sequence,
    }
}

fn person_document(key: &str, state: State) -> Value {
    match (key, state) {
        ("p0", State::Original | State::Updated) => json!({
            "age":20,
            "name":"Alpha",
            "embedding":[1.0,0.0,0.0],
            "position":{"type":"Point","coordinates":[144.0,-38.0]},
            "profile":{"numbers":[1,2,3],"nested":{"enabled":true}},
            "nullable":null
        }),
        ("p1", State::Original) => json!({
            "age":30,
            "name":"Beta",
            "embedding":[0.0,1.0,0.0],
            "position":{"type":"Point","coordinates":[145.0,-37.0]},
            "profile":{"numbers":[4,5,6]}
        }),
        ("p1", State::Updated) => json!({
            "age":35,
            "name":"Beta Updated",
            "embedding":[0.0,1.0,0.0],
            "position":{"type":"Point","coordinates":[145.0,-37.0]},
            "profile":{"numbers":[4,5,6]}
        }),
        ("p2", State::Original) => json!({
            "age":30,
            "name":"Gamma",
            "embedding":[0.0,0.0,1.0],
            "position":{"type":"Point","coordinates":[146.0,-36.0]},
            "profile":{"numbers":[]},
            "nullable":null
        }),
        ("p3", State::Updated) => json!({
            "age":40,
            "name":"Delta",
            "embedding":[0.5,0.5,0.0],
            "position":{"type":"Point","coordinates":[147.0,-35.0]},
            "profile":{"numbers":[7,8,9]}
        }),
        ("p0", State::Roundtrip) => json!({
            "age":22,
            "name":"Alpha Roundtrip",
            "embedding":[1.0,0.0,0.0],
            "position":{"type":"Point","coordinates":[144.0,-38.0]},
            "profile":{"numbers":[1,2,3],"nested":{"enabled":false}},
            "nullable":null
        }),
        ("p3", State::Roundtrip) => person_document("p3", State::Updated),
        ("p4", State::Roundtrip) => json!({
            "age":35,
            "name":"Epsilon",
            "embedding":[0.25,0.75,0.0],
            "position":{"type":"Point","coordinates":[148.0,-34.0]},
            "profile":{"numbers":[10,11,12]}
        }),
        _ => panic!("no expected person document for {key:?}/{state:?}"),
    }
}

fn organization_document(key: &str) -> Value {
    match key {
        "o0" => json!({"name":"Council","profile":{"kind":"public"}}),
        // Reuses a people external key deliberately; identity remains scoped.
        "p0" => json!({"name":"Agency","profile":{"kind":"private"}}),
        _ => panic!("no expected organization document for {key:?}"),
    }
}

fn expected_people(state: State) -> Vec<(EntityId, &'static str, Value)> {
    match state {
        State::Original => vec![
            (entity(1, 1), "p0", person_document("p0", state)),
            (entity(1, 2), "p1", person_document("p1", state)),
            (entity(1, 3), "p2", person_document("p2", state)),
        ],
        State::Updated => vec![
            (entity(1, 1), "p0", person_document("p0", state)),
            (entity(1, 2), "p1", person_document("p1", state)),
            (entity(1, 4), "p3", person_document("p3", state)),
        ],
        State::Roundtrip => vec![
            (entity(1, 1), "p0", person_document("p0", state)),
            (entity(1, 4), "p3", person_document("p3", state)),
            (entity(1, 5), "p4", person_document("p4", state)),
        ],
    }
}

fn expected_organizations() -> Vec<(EntityId, &'static str, Value)> {
    vec![
        (entity(2, 1), "o0", organization_document("o0")),
        (entity(2, 2), "p0", organization_document("p0")),
    ]
}

fn check_rows(
    db: &Database,
    collection: CollectionId,
    expected: Vec<(EntityId, &'static str, Value)>,
) {
    let mut oracle = BTreeMap::new();
    for (id, key, document) in expected {
        let row = db.get(collection, key).unwrap().expect("expected row");
        assert_eq!(row.id, id, "stable identity for {key}");
        assert_eq!(row.key, key);
        assert_eq!(row.document, document, "document for {key}");
        assert_eq!(db.get_by_id(id).unwrap(), Some(row.clone()));
        assert!(oracle.insert(id, (key.to_owned(), document)).is_none());
    }
    let actual = db
        .scan(collection, None)
        .unwrap()
        .map(|row| {
            let row = row.unwrap();
            (row.id, (row.key, row.document))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(actual, oracle, "exact collection rows");
}

fn check_indexes(db: &Database, state: State) {
    let people = CollectionId(1);
    let indexes = db.list_indexes(people).unwrap();
    assert_eq!(indexes.len(), 2);
    assert_eq!(indexes[0].id, IndexId(1));
    assert_eq!(indexes[0].name, "age_idx");
    assert_eq!(indexes[0].field, "age");
    assert_eq!(indexes[0].kind, Kind::Int);
    assert_eq!(indexes[0].state, IndexState::Ready);
    assert_eq!(indexes[1].id, IndexId(2));
    assert_eq!(indexes[1].name, "name_idx");
    assert_eq!(indexes[1].field, "name");
    assert_eq!(indexes[1].kind, Kind::Text);
    assert_eq!(indexes[1].state, IndexState::Ready);

    let (age_all, age_match, match_value, name_all) = match state {
        State::Original => (
            vec![entity(1, 1), entity(1, 2), entity(1, 3)],
            vec![entity(1, 2), entity(1, 3)],
            json!(30),
            vec![entity(1, 1), entity(1, 2), entity(1, 3)],
        ),
        State::Updated => (
            vec![entity(1, 1), entity(1, 2), entity(1, 4)],
            vec![entity(1, 2)],
            json!(35),
            vec![entity(1, 1), entity(1, 2), entity(1, 4)],
        ),
        State::Roundtrip => (
            vec![entity(1, 1), entity(1, 5), entity(1, 4)],
            vec![entity(1, 5)],
            json!(35),
            vec![entity(1, 1), entity(1, 4), entity(1, 5)],
        ),
    };
    assert_eq!(
        db.query_scalar(
            IndexId(1),
            ScalarPredicate::Range {
                lower: None,
                upper: None,
            },
            16,
        )
        .unwrap(),
        age_all
    );
    assert_eq!(
        db.query_scalar(IndexId(1), ScalarPredicate::Eq(match_value), 16)
            .unwrap(),
        age_match
    );
    assert_eq!(
        db.query_scalar(
            IndexId(2),
            ScalarPredicate::Range {
                lower: None,
                upper: None,
            },
            16,
        )
        .unwrap(),
        name_all
    );
}

fn neighbors(
    db: &Database,
    entity: EntityId,
    direction: Direction,
    context: GraphContextId,
    edge_type: EdgeTypeId,
) -> Vec<sekejap_core::collections::Edge> {
    db.neighbors(NeighborRequest {
        entity,
        direction,
        context,
        edge_type: Some(edge_type),
        limit: 16,
    })
    .unwrap()
}

fn check_graph(db: &Database, state: State) {
    let knows = db.edge_type("knows").unwrap().unwrap();
    let member = db.edge_type("member_of").unwrap().unwrap();
    let history = db.graph_context("history").unwrap().unwrap();
    assert_eq!(knows, EdgeTypeId(1));
    assert_eq!(member, EdgeTypeId(2));
    assert_eq!(history, GraphContextId(1));
    assert_eq!(db.graph_context("").unwrap(), Some(GraphContextId::BASE));

    let p0 = entity(1, 1);
    let p1 = entity(1, 2);
    let p2 = entity(1, 3);
    let o0 = entity(2, 1);
    let other_org = entity(2, 2);
    let outgoing = neighbors(db, p0, Direction::Outgoing, GraphContextId::BASE, knows);
    assert_eq!(outgoing.len(), 1);
    assert_eq!(
        outgoing[0].key.destination,
        if state == State::Roundtrip {
            entity(1, 5)
        } else {
            p1
        }
    );
    assert_eq!(
        outgoing[0].properties,
        match state {
            State::Original => json!({"since":u64::MAX,"nested":{"source":"fixture"}}),
            State::Updated => json!({"since":7,"updated":true}),
            State::Roundtrip => json!({"roundtrip":true}),
        }
    );
    assert_eq!(
        neighbors(db, o0, Direction::Incoming, GraphContextId::BASE, member,)[0]
            .key
            .source,
        p0
    );
    assert_eq!(
        neighbors(
            db,
            other_org,
            Direction::Incoming,
            GraphContextId::BASE,
            member,
        )[0]
        .key
        .source,
        if state == State::Roundtrip {
            entity(1, 5)
        } else {
            p1
        }
    );

    let expected_bfs = match state {
        State::Original => vec![(p1, 1), (p2, 2)],
        State::Updated => vec![(p1, 1), (entity(1, 4), 2)],
        State::Roundtrip => vec![(entity(1, 5), 1), (entity(1, 4), 2)],
    };
    let bfs = db
        .traverse_bfs(BfsRequest {
            seed: p0,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            min_depth: 1,
            max_depth: 8,
            include_seed: false,
            max_visited: 32,
            max_edges: 64,
            result_limit: 16,
            edge_where: &[],
            node_where: &[],
        })
        .unwrap();
    assert_eq!(
        bfs.nodes
            .into_iter()
            .map(|node| (node.entity, node.depth))
            .collect::<Vec<_>>(),
        expected_bfs
    );

    if state == State::Original {
        assert_eq!(
            neighbors(db, p0, Direction::Incoming, GraphContextId::BASE, knows,)[0]
                .key
                .source,
            p2
        );
        let historical = neighbors(db, p0, Direction::Outgoing, history, knows);
        assert_eq!(historical[0].key.destination, p2);
        assert_eq!(historical[0].properties, json!({"asserted":"historical"}));
    } else {
        assert!(db.get_by_id(p2).unwrap().is_none());
        assert!(neighbors(db, p0, Direction::Incoming, GraphContextId::BASE, knows).is_empty());
        assert!(neighbors(db, p0, Direction::Outgoing, history, knows).is_empty());
    }
    if state == State::Roundtrip {
        assert!(db.get_by_id(p1).unwrap().is_none());
    }
}

fn check_database(db: &Database, state: State) {
    assert_eq!(db.collection("people").unwrap(), Some(CollectionId(1)));
    assert_eq!(
        db.collection("organizations").unwrap(),
        Some(CollectionId(2))
    );
    let people = db.collection_info(CollectionId(1)).unwrap();
    assert!(!people.timestamps);
    assert_eq!(
        people.layout.fields,
        vec![
            ("age".into(), Kind::Int),
            ("name".into(), Kind::Text),
            ("embedding".into(), Kind::Vector(3)),
            ("position".into(), Kind::Point),
            ("profile".into(), Kind::Json),
        ]
    );
    let organizations = db.collection_info(CollectionId(2)).unwrap();
    assert!(!organizations.timestamps);
    assert_eq!(
        organizations.layout.fields,
        vec![("name".into(), Kind::Text), ("profile".into(), Kind::Json)]
    );
    check_rows(db, CollectionId(1), expected_people(state));
    check_rows(db, CollectionId(2), expected_organizations());
    check_indexes(db, state);
    check_graph(db, state);
}

fn assert_required_features(dir: &Path) {
    let store = PageWalStore::open_snapshot(dir, CACHE_BYTES).unwrap();
    for copy in 0..3u8 {
        let packet = store.get(&[0, 0, copy]).unwrap().unwrap();
        assert_eq!(&packet[..8], b"E4COLL2\0");
        assert_eq!(
            crc32c::crc32c(&packet[..packet.len() - 4]),
            u32::from_le_bytes(packet[packet.len() - 4..].try_into().unwrap())
        );
        assert_eq!(u64::from_be_bytes(packet[18..26].try_into().unwrap()), 3);
    }
}

/// Read only the two replicated physical header pages. `PageRef::open`
/// verifies each page CRC before slot 0 is interpreted.
fn physical_features(dir: &Path) -> [u64; 2] {
    let mut data = fs::File::open(dir.join("data")).unwrap();
    let mut out = [0u64; 2];
    for (page_no, feature) in out.iter_mut().enumerate() {
        let mut page = [0u8; PAGE_SIZE];
        data.read_exact(&mut page).unwrap();
        let header = PageRef::open(&page, page_no as u32).unwrap().slot(0);
        assert_eq!(&header[..8], b"E4PWAL02");
        *feature = u64::from_le_bytes(
            header[PHYSICAL_FEATURES_OFFSET..PHYSICAL_FEATURES_OFFSET + 8]
                .try_into()
                .unwrap(),
        );
    }
    out
}

fn write_manifest(dir: &Path, boundary: &str, physical: [u64; 2]) {
    let manifest = json!({
        "format": FORMAT,
        "status": "candidate-not-released-or-frozen",
        "generator": "bench/src/bin/phase2_format_fixture.rs",
        "source_generation_boundary": boundary,
        "required_logical_features": 3,
        "physical_features": physical,
        "oracle": {
            "collections": 2,
            "original_entities": 5,
            "scalar_indexes": ["age_idx", "name_idx"],
            "edge_types": ["knows", "member_of"],
            "named_contexts": ["history"]
        }
    });
    fs::write(
        dir.join(MANIFEST),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn validate_manifest(path: &Path) -> (Vec<u8>, [u64; 2]) {
    let bytes = fs::read(path).expect("fixture manifest");
    let manifest: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(manifest["format"], FORMAT);
    assert_eq!(manifest["status"], "candidate-not-released-or-frozen");
    assert_eq!(manifest["required_logical_features"], 3);
    let physical: [u64; 2] = serde_json::from_value(manifest["physical_features"].clone()).unwrap();
    (bytes, physical)
}

fn known_fixture_file(name: &str) -> bool {
    matches!(
        name,
        "data"
            | "wal"
            | "writer.lock"
            | "readers.lock"
            | "reader-0.lock"
            | "reader-1.lock"
            | "reader-2.lock"
            | "reader-3.lock"
            | "reader-4.lock"
            | "reader-5.lock"
            | "reader-6.lock"
            | "reader-7.lock"
            | MANIFEST
    )
}

/// Resolve and validate the copy without opening any database file. This
/// prevents a nominal copy from writing through a symlink or hard link into
/// preserved source evidence.
fn guarded_upgrade_paths(source_manifest: &Path, copied: &Path) -> (PathBuf, PathBuf) {
    let source_input = fs::symlink_metadata(source_manifest).expect("source manifest metadata");
    assert!(
        source_input.file_type().is_file() && !source_input.file_type().is_symlink(),
        "source manifest must be an ordinary file, not a symlink"
    );
    let copied_input = fs::symlink_metadata(copied).expect("copied database metadata");
    assert!(
        copied_input.file_type().is_dir() && !copied_input.file_type().is_symlink(),
        "copied database must be an ordinary directory, not a symlink"
    );
    let source_manifest = fs::canonicalize(source_manifest).expect("source manifest");
    let copied = fs::canonicalize(copied).expect("copied database");
    let source_dir = source_manifest.parent().unwrap();
    assert_ne!(source_dir, copied, "refusing to mutate source fixture");
    assert!(
        !source_manifest.starts_with(&copied),
        "source manifest must be outside copied target"
    );
    assert!(
        !copied.starts_with(source_dir),
        "copied target must be outside source fixture directory"
    );

    #[cfg(unix)]
    let source_identities = fs::read_dir(source_dir)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            (metadata.file_type().is_file() && !metadata.file_type().is_symlink())
                .then_some((metadata.dev(), metadata.ino()))
        })
        .collect::<Vec<_>>();

    let mut essential = [false; 4];
    for entry in fs::read_dir(&copied).expect("copied fixture inventory") {
        let entry = entry.unwrap();
        let name = entry
            .file_name()
            .into_string()
            .expect("fixture filenames must be UTF-8");
        assert!(
            known_fixture_file(&name),
            "unexpected copied fixture entry {name:?}"
        );
        let metadata = fs::symlink_metadata(entry.path()).unwrap();
        assert!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "copied fixture entry {name:?} must be an ordinary file"
        );
        match name.as_str() {
            "data" => essential[0] = true,
            "wal" => essential[1] = true,
            "writer.lock" => essential[2] = true,
            MANIFEST => essential[3] = true,
            _ => {}
        }
        #[cfg(unix)]
        {
            assert_eq!(
                metadata.nlink(),
                1,
                "copied fixture entry {name:?} has multiple hard links"
            );
            assert!(
                !source_identities.contains(&(metadata.dev(), metadata.ino())),
                "copied fixture entry {name:?} aliases a source inode"
            );
        }
    }
    assert!(
        essential.into_iter().all(|present| present),
        "copied fixture is incomplete"
    );
    (source_manifest, copied)
}

fn populate(db: &mut Database) {
    let people = db
        .create_collection(
            "people",
            vec![
                ("age".into(), Kind::Int),
                ("name".into(), Kind::Text),
                ("embedding".into(), Kind::Vector(3)),
                ("position".into(), Kind::Point),
                ("profile".into(), Kind::Json),
            ],
            CollectionOptions { timestamps: false },
        )
        .unwrap();
    let organizations = db
        .create_collection(
            "organizations",
            vec![("name".into(), Kind::Text), ("profile".into(), Kind::Json)],
            CollectionOptions { timestamps: false },
        )
        .unwrap();
    assert_eq!(people, CollectionId(1));
    assert_eq!(organizations, CollectionId(2));
    for (id, key, document) in expected_people(State::Original) {
        assert_eq!(db.put(people, key, &document).unwrap(), id);
    }
    for (id, key, document) in expected_organizations() {
        assert_eq!(db.put(organizations, key, &document).unwrap(), id);
    }
    let age = db
        .create_scalar_index(people, "age_idx", "age", false)
        .unwrap();
    let name = db
        .create_scalar_index(people, "name_idx", "name", false)
        .unwrap();
    assert_eq!((age, name), (IndexId(1), IndexId(2)));
    assert!(db.build_index_step(age, 16).unwrap());
    assert!(db.build_index_step(name, 16).unwrap());

    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let member = db.create_edge_type("member_of").unwrap();
    let history = db.create_graph_context("history").unwrap();
    assert_eq!(
        (knows, member, history),
        (EdgeTypeId(1), EdgeTypeId(2), GraphContextId(1))
    );
    let p0 = entity(1, 1);
    let p1 = entity(1, 2);
    let p2 = entity(1, 3);
    db.put_edge(
        GraphContextId::BASE,
        p0,
        knows,
        p1,
        &json!({"since":u64::MAX,"nested":{"source":"fixture"}}),
    )
    .unwrap();
    db.put_edge(GraphContextId::BASE, p1, knows, p2, &json!({"step":2}))
        .unwrap();
    db.put_edge(GraphContextId::BASE, p2, knows, p0, &json!({"step":3}))
        .unwrap();
    db.put_edge(history, p0, knows, p2, &json!({"asserted":"historical"}))
        .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        p0,
        member,
        entity(2, 1),
        &json!({"role":"member"}),
    )
    .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        p1,
        member,
        entity(2, 2),
        &json!({"role":"member"}),
    )
    .unwrap();
}

fn check_boundary(dir: &Path, boundary: &str) {
    let wal_empty = fs::metadata(dir.join("wal")).unwrap().len() == 0;
    assert_eq!(
        wal_empty,
        boundary == "checkpointed",
        "WAL handoff boundary"
    );
}

fn generate(dir: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    assert!(!dir.exists(), "generate destination must be new");
    let mut db = Database::create(dir, cfg()).unwrap();
    populate(&mut db);
    db.commit().unwrap();
    check_database(&db, State::Original);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    check_boundary(dir, boundary);
    assert_required_features(dir);
    let physical = physical_features(dir);
    assert_eq!(
        physical,
        [u64::from(create_compact_cells()); 2],
        "created physical features differ from configured create default"
    );
    write_manifest(dir, boundary, physical);
}

fn verify(dir: &Path, state: State) {
    let (_, expected_physical) = validate_manifest(&dir.join(MANIFEST));
    assert_eq!(
        physical_features(dir),
        expected_physical,
        "physical features changed"
    );
    let db = Database::open_snapshot(dir, cfg()).unwrap();
    check_database(&db, state);
    drop(db);
    assert_required_features(dir);
}

fn upgrade(source_manifest: &Path, copied: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    let (source_manifest, copied) = guarded_upgrade_paths(source_manifest, copied);
    let (source_bytes, expected_physical) = validate_manifest(&source_manifest);
    assert_eq!(
        fs::read(copied.join(MANIFEST)).unwrap(),
        source_bytes,
        "copied target manifest differs from explicit source"
    );
    assert_eq!(
        physical_features(&copied),
        expected_physical,
        "copied target physical features differ from source manifest"
    );
    verify(&copied, State::Original);
    let mut db = Database::open(&copied, cfg()).unwrap();
    let old = Database::open_snapshot(&copied, cfg()).unwrap();
    let people = CollectionId(1);
    assert_eq!(
        db.update(people, "p1", &json!({"age":35,"name":"Beta Updated"}))
            .unwrap(),
        entity(1, 2)
    );
    let knows = EdgeTypeId(1);
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 1),
        knows,
        entity(1, 2),
        &json!({"since":7,"updated":true}),
    )
    .unwrap();
    assert!(db.delete(people, "p2").unwrap());
    assert_eq!(
        db.put(people, "p3", &person_document("p3", State::Updated))
            .unwrap(),
        entity(1, 4)
    );
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 2),
        knows,
        entity(1, 4),
        &json!({"inserted":true}),
    )
    .unwrap();
    db.commit().unwrap();
    check_database(&db, State::Updated);
    check_database(&old, State::Original);
    drop(old);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    check_boundary(&copied, boundary);
    assert_required_features(&copied);
    assert_eq!(
        physical_features(&copied),
        expected_physical,
        "upgrade changed physical features"
    );
    // No typed reopen: a later binary should be first to admit these bytes.
}

fn continue_upgrade(source_manifest: &Path, copied: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    let (source_manifest, copied) = guarded_upgrade_paths(source_manifest, copied);
    let (source_bytes, expected_physical) = validate_manifest(&source_manifest);
    assert_eq!(
        fs::read(copied.join(MANIFEST)).unwrap(),
        source_bytes,
        "copied target manifest differs from explicit source"
    );
    assert_eq!(physical_features(&copied), expected_physical);
    verify(&copied, State::Updated);

    let mut db = Database::open(&copied, cfg()).unwrap();
    let old = Database::open_snapshot(&copied, cfg()).unwrap();
    let people = CollectionId(1);
    assert_eq!(
        db.update(people, "p0", &person_document("p0", State::Roundtrip))
            .unwrap(),
        entity(1, 1)
    );
    assert!(db.delete(people, "p1").unwrap());
    assert_eq!(
        db.put(people, "p4", &person_document("p4", State::Roundtrip))
            .unwrap(),
        entity(1, 5)
    );
    let knows = EdgeTypeId(1);
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 1),
        knows,
        entity(1, 5),
        &json!({"roundtrip":true}),
    )
    .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 5),
        knows,
        entity(1, 4),
        &json!({"bridge":"old-writer"}),
    )
    .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 5),
        EdgeTypeId(2),
        entity(2, 2),
        &json!({"role":"roundtrip"}),
    )
    .unwrap();
    db.commit().unwrap();
    check_database(&db, State::Roundtrip);
    check_database(&old, State::Updated);
    drop(old);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    check_boundary(&copied, boundary);
    assert_required_features(&copied);
    assert_eq!(physical_features(&copied), expected_physical);
    // A later current binary must be first to reopen the old writer's bytes.
}

fn verify_writer(source_manifest: &Path, copied: &Path, state: State) {
    assert_eq!(
        state,
        State::Roundtrip,
        "verify-writer requires roundtrip state"
    );
    let (source_manifest, copied) = guarded_upgrade_paths(source_manifest, copied);
    let (source_bytes, expected_physical) = validate_manifest(&source_manifest);
    assert_eq!(fs::read(copied.join(MANIFEST)).unwrap(), source_bytes);
    assert_eq!(physical_features(&copied), expected_physical);
    verify(&copied, state);
    let db = Database::open(&copied, cfg()).unwrap();
    check_database(&db, state);
    drop(db);
    assert_required_features(&copied);
    assert_eq!(physical_features(&copied), expected_physical);
}

fn parse_state(value: &str) -> State {
    match value {
        "original" => State::Original,
        "updated" => State::Updated,
        "roundtrip" => State::Roundtrip,
        _ => panic!("state must be original|updated|roundtrip"),
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.as_slice() == ["--version"] {
        println!(
            "{}",
            json!({
                "harness":"phase2-format-fixture-v1",
                "rollback_cycle_version":1,
                "engine_revision":option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded"),
                "create_compact_cells":create_compact_cells(),
                "compile_features":{
                    "compact-cells":cfg!(feature="compact-cells"),
                    "sqlite-balance":cfg!(feature="sqlite-balance"),
                    "keyspace-append":cfg!(feature="keyspace-append"),
                    "slotref-split":cfg!(feature="slotref-split")
                }
            })
        );
        return;
    }
    match args.as_slice() {
        [command, dir, boundary] if command == "generate" => {
            generate(Path::new(dir), boundary);
            println!(
                "{}",
                json!({"result":"PASS","command":"generate","database":dir,"boundary":boundary})
            );
        }
        [command, dir, state] if command == "verify" => {
            verify(Path::new(dir), parse_state(state));
            println!(
                "{}",
                json!({"result":"PASS","command":"verify","database":dir,"state":state})
            );
        }
        [command, source, copied, boundary, confirm]
            if command == "upgrade" && confirm == "--confirm-copy" =>
        {
            upgrade(Path::new(source), Path::new(copied), boundary);
            println!(
                "{}",
                json!({"result":"PASS","command":"upgrade","database":copied,"boundary":boundary})
            );
        }
        [command, source, copied, boundary, confirm]
            if command == "continue-upgrade" && confirm == "--confirm-copy" =>
        {
            continue_upgrade(Path::new(source), Path::new(copied), boundary);
            println!(
                "{}",
                json!({"result":"PASS","command":"continue-upgrade","database":copied,"boundary":boundary})
            );
        }
        [command, source, copied, state, confirm]
            if command == "verify-writer" && confirm == "--confirm-copy" =>
        {
            verify_writer(Path::new(source), Path::new(copied), parse_state(state));
            println!(
                "{}",
                json!({"result":"PASS","command":"verify-writer","database":copied,"state":state})
            );
        }
        _ => panic!(
            "usage:\n  phase2_format_fixture generate NEW_DIR checkpointed|wal-pending\n  phase2_format_fixture verify EXISTING_DIR original|updated|roundtrip\n  phase2_format_fixture upgrade SOURCE_MANIFEST COPIED_DIR checkpointed|wal-pending --confirm-copy\n  phase2_format_fixture continue-upgrade SOURCE_MANIFEST COPIED_DIR checkpointed|wal-pending --confirm-copy\n  phase2_format_fixture verify-writer SOURCE_MANIFEST COPIED_DIR roundtrip --confirm-copy"
        ),
    }
}
