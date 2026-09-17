//! Linux-only child process and oracle helper for Phase-2 crash qualification.
//! Process timing arms are observational; deterministic checkpoint stages use
//! the PageWAL pilot crash seam and lifecycle arms use explicit barriers.
use e4_prototype::{
    collections::{
        ApproxVectorMethod, BfsRequest, CollectionId, CollectionOptions, Database, Direction,
        EdgeTypeId, EntityId, GraphContextId, IndexFamily, IndexId, IndexState,
        QuantizedVectorCandidates, ScalarPredicate, SpatialCandidates, TextCandidates, TextMatch,
        VectorCandidates, VectorMetric,
    },
    pagewal::{create_compact_cells, PageWalStore},
    spatial_math::{Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    page::{PageRef, PAGE_SIZE},
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, BufRead, Read, Write},
    path::Path,
};

const CACHE_BYTES: usize = 1 << 20;
const MANIFEST: &str = "MULTIMODEL_FIXTURE.json";
const FORMAT: &str = "e4-phase2-multimodel-candidate-v2";
const PHYSICAL_FEATURES_OFFSET: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Original,
    Updated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    Vector,
    Quantized,
    Spatial,
    Text,
    All,
}

impl Profile {
    fn parse(value: &str) -> Self {
        match value {
            "vector" => Self::Vector,
            "quantized" => Self::Quantized,
            "spatial" => Self::Spatial,
            "text" => Self::Text,
            "all" => Self::All,
            _ => panic!("profile must be vector|quantized|spatial|text|all"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Quantized => "quantized",
            Self::Spatial => "spatial",
            Self::Text => "text",
            Self::All => "all",
        }
    }

    fn vector(self) -> bool {
        matches!(self, Self::Vector | Self::All)
    }

    fn quantized(self) -> bool {
        matches!(self, Self::Quantized | Self::All)
    }

    fn spatial(self) -> bool {
        matches!(self, Self::Spatial | Self::All)
    }

    fn text(self) -> bool {
        matches!(self, Self::Text | Self::All)
    }

    fn logical_features(self) -> u64 {
        3 | if self.vector() { 4 } else { 0 }
            | if self.spatial() { 8 } else { 0 }
            | if self.text() { 16 } else { 0 }
            | if self.quantized() { 32 } else { 0 }
    }
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
        ("p0", State::Original) => json!({
            "age":20,
            "name":"Alpha",
            "embedding":[1.0,0.0],
            "position":{"type":"Point","coordinates":[0.0,0.0]},
            "body":"Rust database database İstanbul",
            "profile":{"numbers":[1,2,3],"nested":{"enabled":true}}
        }),
        ("p0", State::Updated) => json!({
            "age":25,
            "name":"Alpha Updated",
            "embedding":[0.0,1.0],
            "position":{"type":"Point","coordinates":[10.0,0.0]},
            "body":"updated delta",
            "profile":{"numbers":[1,2,3],"nested":{"enabled":false}}
        }),
        ("p1", _) => json!({
            "age":30,
            "name":"Beta",
            "embedding":[1.0,0.0],
            "position":{"type":"Point","coordinates":[1.0,0.0]},
            "body":"Rust search CAFÉ",
            "profile":{"numbers":[4,5,6]}
        }),
        ("p2", State::Original) => json!({
            "age":40,
            "name":"Gamma",
            "embedding":[0.0,1.0],
            "position":{"type":"Point","coordinates":[2.0,0.0]},
            "body":"database search 北京",
            "profile":{"numbers":[]}
        }),
        ("p3", State::Updated) => json!({
            "age":35,
            "name":"Delta",
            "embedding":[1.0,0.0],
            "position":{"type":"Point","coordinates":[0.5,0.0]},
            "body":"rust database café 北京",
            "profile":{"numbers":[7,8,9]}
        }),
        _ => panic!("no expected person document for {key:?}/{state:?}"),
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
    }
}

fn organization_document() -> Value {
    json!({"name":"Council","profile":{"kind":"public"}})
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

fn vector_index(profile: Profile) -> Option<IndexId> {
    profile.vector().then_some(IndexId(2))
}

fn quantized_index(profile: Profile) -> Option<IndexId> {
    profile
        .quantized()
        .then_some(IndexId(2 + u64::from(profile.vector())))
}

fn spatial_index(profile: Profile) -> Option<IndexId> {
    profile.spatial().then_some(IndexId(
        2 + u64::from(profile.vector()) + u64::from(profile.quantized()),
    ))
}

fn text_index(profile: Profile) -> Option<IndexId> {
    profile.text().then_some(IndexId(
        2 + u64::from(profile.vector())
            + u64::from(profile.quantized())
            + u64::from(profile.spatial()),
    ))
}

fn check_indexes(db: &Database, profile: Profile, state: State) {
    let indexes = db.list_indexes(CollectionId(1)).unwrap();
    assert!(
        indexes.len()
            >= 1 + usize::from(profile.vector())
                + usize::from(profile.quantized())
                + usize::from(profile.spatial())
                + usize::from(profile.text())
    );
    assert_eq!(indexes[0].id, IndexId(1));
    assert_eq!(indexes[0].name, "age_idx");
    assert_eq!(indexes[0].field, "age");
    assert_eq!(indexes[0].kind, Kind::Int);
    assert!(!indexes[0].unique);
    assert_eq!(indexes[0].family, IndexFamily::Scalar);
    assert_eq!(indexes[0].state, IndexState::Ready);
    let mut at = 1;
    if profile.vector() {
        assert_eq!(indexes[at].id, vector_index(profile).unwrap());
        assert_eq!(indexes[at].name, "embedding_exact");
        assert_eq!(indexes[at].field, "embedding");
        assert_eq!(indexes[at].kind, Kind::Vector(2));
        assert_eq!(indexes[at].family, IndexFamily::ExactVector);
        assert_eq!(indexes[at].state, IndexState::Ready);
        at += 1;
    }
    if profile.quantized() {
        assert_eq!(indexes[at].id, quantized_index(profile).unwrap());
        assert_eq!(indexes[at].name, "embedding_int8");
        assert_eq!(indexes[at].field, "embedding");
        assert_eq!(indexes[at].kind, Kind::Vector(2));
        assert_eq!(indexes[at].family, IndexFamily::QuantizedVector);
        assert_eq!(indexes[at].state, IndexState::Ready);
        at += 1;
    }
    if profile.spatial() {
        assert_eq!(indexes[at].id, spatial_index(profile).unwrap());
        assert_eq!(indexes[at].name, "position_point");
        assert_eq!(indexes[at].field, "position");
        assert_eq!(indexes[at].kind, Kind::Point);
        assert_eq!(indexes[at].family, IndexFamily::SpatialPoint);
        assert_eq!(indexes[at].state, IndexState::Ready);
        at += 1;
    }
    if profile.text() {
        assert_eq!(indexes[at].id, text_index(profile).unwrap());
        assert_eq!(indexes[at].name, "body_text");
        assert_eq!(indexes[at].field, "body");
        assert_eq!(indexes[at].kind, Kind::Text);
        assert_eq!(indexes[at].family, IndexFamily::Text);
        assert_eq!(indexes[at].state, IndexState::Ready);
    }

    let (all, equal) = match state {
        State::Original => (
            vec![entity(1, 1), entity(1, 2), entity(1, 3)],
            vec![entity(1, 2)],
        ),
        State::Updated => (
            vec![entity(1, 1), entity(1, 2), entity(1, 4)],
            vec![entity(1, 2)],
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
        all
    );
    assert_eq!(
        db.query_scalar(IndexId(1), ScalarPredicate::Eq(json!(30)), 16)
            .unwrap(),
        equal
    );
}

fn assert_vector_hits(
    db: &Database,
    profile: Profile,
    state: State,
    metric: VectorMetric,
    expected: &[(EntityId, f64)],
) {
    let hits = db
        .query_exact_vector(
            vector_index(profile).unwrap(),
            &[1.0, 0.0],
            metric,
            8,
            VectorCandidates::All,
            8,
            || false,
        )
        .unwrap();
    assert_eq!(hits.len(), expected.len(), "{state:?}/{metric:?}");
    for (hit, &(id, distance)) in hits.iter().zip(expected) {
        assert_eq!(hit.id, id);
        assert!((hit.distance - distance).abs() <= 1e-15);
    }
}

fn check_vector(db: &Database, profile: Profile, state: State) {
    if !profile.vector() {
        return;
    }
    let expected = match state {
        State::Original => vec![
            (entity(1, 1), 0.0),
            (entity(1, 2), 0.0),
            (entity(1, 3), 2.0),
        ],
        State::Updated => vec![
            (entity(1, 2), 0.0),
            (entity(1, 4), 0.0),
            (entity(1, 1), 2.0),
        ],
    };
    assert_vector_hits(db, profile, state, VectorMetric::SquaredL2, &expected);
    let cosine = expected
        .iter()
        .map(|&(id, distance)| (id, if distance == 0.0 { 0.0 } else { 1.0 }))
        .collect::<Vec<_>>();
    assert_vector_hits(db, profile, state, VectorMetric::Cosine, &cosine);
}

fn check_quantized_vector(db: &Database, profile: Profile, state: State) {
    if !profile.quantized() {
        return;
    }
    let expected = match state {
        State::Original => vec![
            (entity(1, 1), 0.0),
            (entity(1, 2), 0.0),
            (entity(1, 3), 2.0),
        ],
        State::Updated => vec![
            (entity(1, 2), 0.0),
            (entity(1, 4), 0.0),
            (entity(1, 1), 2.0),
        ],
    };
    let result = db
        .query_quantized_vector(
            quantized_index(profile).unwrap(),
            &[1.0, 0.0],
            VectorMetric::SquaredL2,
            8,
            8,
            QuantizedVectorCandidates::All,
            8,
            || false,
        )
        .unwrap();
    assert_eq!(result.method, ApproxVectorMethod::SymmetricInt8ScanV1);
    assert_eq!(result.ef, 8);
    assert_eq!(result.examined, expected.len());
    assert_eq!(result.reranked, expected.len());
    assert_eq!(result.hits.len(), expected.len());
    for (hit, &(id, distance)) in result.hits.iter().zip(&expected) {
        assert_eq!(hit.id, id);
        assert!((hit.distance - distance).abs() <= 1e-15);
    }

    // With one approximate candidate, the winner depends on the persisted
    // quantized code. This catches a torn row/sidecar/code update that a full
    // candidate scan followed by exact reranking could conceal.
    let selective = db
        .query_quantized_vector(
            quantized_index(profile).unwrap(),
            &[0.0, 1.0],
            VectorMetric::SquaredL2,
            1,
            1,
            QuantizedVectorCandidates::All,
            8,
            || false,
        )
        .unwrap();
    assert_eq!(selective.examined, expected.len());
    assert_eq!(selective.reranked, 1);
    assert_eq!(selective.hits.len(), 1);
    assert_eq!(
        selective.hits[0].id,
        match state {
            State::Original => entity(1, 3),
            State::Updated => entity(1, 1),
        }
    );
    assert!(selective.hits[0].distance.abs() <= 1e-15);
}

fn check_spatial(db: &Database, profile: Profile, state: State) {
    if !profile.spatial() {
        return;
    }
    let index = spatial_index(profile).unwrap();
    let expected = match state {
        State::Original => vec![entity(1, 1), entity(1, 2)],
        State::Updated => vec![entity(1, 2), entity(1, 4)],
    };
    assert_eq!(
        db.query_point_bbox(
            index,
            Bounds::new(-0.1, 1.1, -0.1, 0.1).unwrap(),
            8,
            SpatialCandidates::All,
            8,
            || false,
        )
        .unwrap(),
        expected
    );
    assert_eq!(
        db.query_point_radius(
            index,
            Point::new(0.0, 0.0).unwrap(),
            120_000.0,
            8,
            SpatialCandidates::All,
            8,
            || false,
        )
        .unwrap(),
        expected
    );
}

fn assert_text_hits(db: &Database, index: IndexId, query: &str, expected: &[(EntityId, f64)]) {
    let hits = db
        .query_text(
            index,
            query,
            TextMatch::Any,
            8,
            TextCandidates::All,
            32,
            || false,
        )
        .unwrap();
    assert_eq!(hits.len(), expected.len(), "text query {query:?}");
    for (hit, &(id, score)) in hits.iter().zip(expected) {
        assert_eq!(hit.id, id);
        assert!((hit.score - score).abs() <= 1e-14, "{query:?}: {hit:?}");
    }
}

fn check_text(db: &Database, profile: Profile, state: State) {
    if !profile.text() {
        return;
    }
    let index = text_index(profile).unwrap();
    let (ordinary, unicode) = match state {
        State::Original => (
            vec![
                (entity(1, 1), 1.0462961802661026),
                (entity(1, 2), 0.4900511774126154),
                (entity(1, 3), 0.4900511774126154),
            ],
            vec![
                (entity(1, 2), 1.0226655718605677),
                (entity(1, 3), 1.0226655718605677),
                (entity(1, 1), 0.9066488893385707),
            ],
        ),
        State::Updated => (
            vec![
                (entity(1, 4), 1.2767329363865667),
                (entity(1, 2), 0.47000362924573563),
            ],
            vec![
                (entity(1, 4), 1.2767329363865667),
                (entity(1, 2), 0.47000362924573563),
            ],
        ),
    };
    assert_text_hits(db, index, "rust database", &ordinary);
    assert_text_hits(db, index, "İSTANBUL café 北京", &unicode);
}

fn neighbors(db: &Database, id: EntityId) -> Vec<EntityId> {
    db.neighbors(e4_prototype::collections::NeighborRequest {
        entity: id,
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(EdgeTypeId(1)),
        limit: 16,
    })
    .unwrap()
    .into_iter()
    .map(|edge| edge.key.destination)
    .collect()
}

fn check_graph(db: &Database, state: State) {
    assert_eq!(db.edge_type("knows").unwrap(), Some(EdgeTypeId(1)));
    assert_eq!(db.edge_type("member_of").unwrap(), Some(EdgeTypeId(2)));
    let p0_knows = db
        .neighbors(e4_prototype::collections::NeighborRequest {
            entity: entity(1, 1),
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(EdgeTypeId(1)),
            limit: 16,
        })
        .unwrap();
    assert_eq!(p0_knows.len(), 1);
    assert_eq!(p0_knows[0].key.destination, entity(1, 2));
    assert_eq!(
        p0_knows[0].properties,
        match state {
            State::Original => json!({"since":1}),
            State::Updated => json!({"since":7,"updated":true}),
        }
    );
    let membership = db
        .neighbors(e4_prototype::collections::NeighborRequest {
            entity: entity(1, 1),
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(EdgeTypeId(2)),
            limit: 16,
        })
        .unwrap();
    assert_eq!(membership.len(), 1);
    assert_eq!(membership[0].key.destination, entity(2, 1));
    assert_eq!(membership[0].properties, json!({"role":"member"}));
    assert_eq!(
        neighbors(db, entity(1, 2)),
        match state {
            State::Original => vec![entity(1, 3)],
            State::Updated => vec![entity(1, 4)],
        }
    );
    let bfs = db
        .traverse_bfs(BfsRequest {
            seed: entity(1, 1),
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(EdgeTypeId(1)),
            min_depth: 1,
            max_depth: 4,
            include_seed: false,
            max_visited: 16,
            max_edges: 16,
            result_limit: 16,
        })
        .unwrap();
    assert_eq!(
        bfs.nodes
            .into_iter()
            .map(|node| (node.entity, node.depth))
            .collect::<Vec<_>>(),
        match state {
            State::Original => vec![(entity(1, 2), 1), (entity(1, 3), 2)],
            State::Updated => vec![(entity(1, 2), 1), (entity(1, 4), 2)],
        }
    );
}

fn check_database(db: &Database, profile: Profile, state: State) {
    assert_eq!(db.collection("people").unwrap(), Some(CollectionId(1)));
    assert_eq!(
        db.collection("organizations").unwrap(),
        Some(CollectionId(2))
    );
    assert_eq!(
        db.collection_info(CollectionId(1)).unwrap().layout.fields,
        vec![
            ("age".into(), Kind::Int),
            ("name".into(), Kind::Text),
            ("embedding".into(), Kind::Vector(2)),
            ("position".into(), Kind::Point),
            ("body".into(), Kind::Text),
            ("profile".into(), Kind::Json),
        ]
    );
    check_rows(db, CollectionId(1), expected_people(state));
    check_rows(
        db,
        CollectionId(2),
        vec![(entity(2, 1), "o0", organization_document())],
    );
    check_indexes(db, profile, state);
    check_graph(db, state);
    check_vector(db, profile, state);
    check_quantized_vector(db, profile, state);
    check_spatial(db, profile, state);
    check_text(db, profile, state);
}

fn assert_required_features(dir: &Path, profile: Profile) {
    let store = PageWalStore::open_snapshot(dir, CACHE_BYTES).unwrap();
    for copy in 0..3u8 {
        let packet = store.get(&[0, 0, copy]).unwrap().unwrap();
        assert_eq!(&packet[..8], b"E4COLL2\0");
        assert_eq!(
            crc32c::crc32c(&packet[..packet.len() - 4]),
            u32::from_le_bytes(packet[packet.len() - 4..].try_into().unwrap())
        );
        assert_eq!(
            u64::from_be_bytes(packet[18..26].try_into().unwrap()),
            profile.logical_features()
        );
    }
}

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

fn manifest_oracle(profile: Profile) -> Value {
    json!({
        "collections":2,
        "original_rows":{"people":[[1,1,"p0"],[1,2,"p1"],[1,3,"p2"]],"organizations":[[2,1,"o0"]]},
        "updated_rows":{"people":[[1,1,"p0"],[1,2,"p1"],[1,4,"p3"]],"organizations":[[2,1,"o0"]]},
        "scalar":{"original_all":[[1,1],[1,2],[1,3]],"updated_all":[[1,1],[1,2],[1,4]],"age_30":[[1,2]]},
        "graph":{"original_bfs":[[[1,2],1],[[1,3],2]],"updated_bfs":[[[1,2],1],[[1,4],2]]},
        "vector":profile.vector().then(|| json!({
            "query":[1.0,0.0],
            "original_squared_l2":[[[1,1],0.0],[[1,2],0.0],[[1,3],2.0]],
            "updated_squared_l2":[[[1,2],0.0],[[1,4],0.0],[[1,1],2.0]]
        })),
        "quantized_vector":profile.quantized().then(|| json!({
            "method":"symmetric-int8-scan-v1",
            "query":[1.0,0.0],
            "ef":8,
            "original_exact_rerank_squared_l2":[[[1,1],0.0],[[1,2],0.0],[[1,3],2.0]],
            "updated_exact_rerank_squared_l2":[[[1,2],0.0],[[1,4],0.0],[[1,1],2.0]],
            "selective_query":[0.0,1.0],
            "selective_ef":1,
            "original_selective_winner":[1,3],
            "updated_selective_winner":[1,1]
        })),
        "spatial":profile.spatial().then(|| json!({
            "bbox":[-0.1,1.1,-0.1,0.1],"radius_metres":120000.0,
            "original":[[1,1],[1,2]],"updated":[[1,2],[1,4]]
        })),
        "text":profile.text().then(|| json!({
            "ordinary_query":"rust database",
            "unicode_query":"İSTANBUL café 北京",
            "original_ordinary":[[[1,1],1.0462961802661026],[[1,2],0.4900511774126154],[[1,3],0.4900511774126154]],
            "updated_ordinary":[[[1,4],1.2767329363865667],[[1,2],0.47000362924573563]],
            "original_unicode":[[[1,2],1.0226655718605677],[[1,3],1.0226655718605677],[[1,1],0.9066488893385707]],
            "updated_unicode":[[[1,4],1.2767329363865667],[[1,2],0.47000362924573563]]
        }))
    })
}

fn write_manifest(dir: &Path, profile: Profile, boundary: &str, physical: [u64; 2]) {
    let manifest = json!({
        "format":FORMAT,
        "status":"candidate-not-released-or-frozen",
        "generator":"src/bin/phase2_crash_probe.rs",
        "profile":profile.name(),
        "source_generation_boundary":boundary,
        "required_logical_features":profile.logical_features(),
        "physical_features":physical,
        "oracle":manifest_oracle(profile)
    });
    fs::write(
        dir.join(MANIFEST),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn finish_build(db: &mut Database, index: IndexId) {
    while !db.build_index_step(index, 2).unwrap() {
        db.commit().unwrap();
    }
}

fn populate(db: &mut Database, profile: Profile) {
    let people = db
        .create_collection(
            "people",
            vec![
                ("age".into(), Kind::Int),
                ("name".into(), Kind::Text),
                ("embedding".into(), Kind::Vector(2)),
                ("position".into(), Kind::Point),
                ("body".into(), Kind::Text),
                ("profile".into(), Kind::Json),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let organizations = db
        .create_collection(
            "organizations",
            vec![("name".into(), Kind::Text), ("profile".into(), Kind::Json)],
            CollectionOptions::default(),
        )
        .unwrap();
    assert_eq!((people, organizations), (CollectionId(1), CollectionId(2)));
    for (id, key, document) in expected_people(State::Original) {
        assert_eq!(db.put(people, key, &document).unwrap(), id);
    }
    assert_eq!(
        db.put(organizations, "o0", &organization_document())
            .unwrap(),
        entity(2, 1)
    );

    let scalar = db
        .create_scalar_index(people, "age_idx", "age", false)
        .unwrap();
    assert_eq!(scalar, IndexId(1));
    finish_build(db, scalar);
    db.enable_graph().unwrap();
    assert_eq!(db.create_edge_type("knows").unwrap(), EdgeTypeId(1));
    assert_eq!(db.create_edge_type("member_of").unwrap(), EdgeTypeId(2));
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 1),
        EdgeTypeId(1),
        entity(1, 2),
        &json!({"since":1}),
    )
    .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 2),
        EdgeTypeId(1),
        entity(1, 3),
        &json!({"since":2}),
    )
    .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 1),
        EdgeTypeId(2),
        entity(2, 1),
        &json!({"role":"member"}),
    )
    .unwrap();

    if profile.vector() {
        let index = db
            .create_exact_vector_index(people, "embedding_exact", "embedding")
            .unwrap();
        assert_eq!(index, vector_index(profile).unwrap());
        finish_build(db, index);
    }
    if profile.quantized() {
        let index = db
            .create_quantized_vector_index(people, "embedding_int8", "embedding")
            .unwrap();
        assert_eq!(index, quantized_index(profile).unwrap());
        finish_build(db, index);
    }
    if profile.spatial() {
        let index = db
            .create_point_index(people, "position_point", "position")
            .unwrap();
        assert_eq!(index, spatial_index(profile).unwrap());
        finish_build(db, index);
    }
    if profile.text() {
        let index = db.create_text_index(people, "body_text", "body").unwrap();
        assert_eq!(index, text_index(profile).unwrap());
        finish_build(db, index);
    }
}

fn check_boundary(dir: &Path, boundary: &str) {
    let wal_empty = fs::metadata(dir.join("wal")).unwrap().len() == 0;
    assert_eq!(
        wal_empty,
        boundary == "checkpointed",
        "WAL handoff boundary"
    );
}

fn generate(profile: Profile, dir: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    assert!(!dir.exists(), "generate destination must be new");
    let mut db = Database::create(dir, cfg()).unwrap();
    populate(&mut db, profile);
    db.commit().unwrap();
    check_database(&db, profile, State::Original);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    check_boundary(dir, boundary);
    assert_required_features(dir, profile);
    let physical = physical_features(dir);
    assert_eq!(physical, [u64::from(create_compact_cells()); 2]);
    write_manifest(dir, profile, boundary, physical);
}

fn parse_state(value: &str) -> State {
    match value {
        "original" => State::Original,
        "updated" => State::Updated,
        _ => panic!("state must be original|updated"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LifecycleFamily {
    Scalar,
    Vector,
    Quantized,
    Spatial,
    Text,
}

impl LifecycleFamily {
    fn parse(value: &str) -> Self {
        match value {
            "scalar" => Self::Scalar,
            "vector" => Self::Vector,
            "quantized" => Self::Quantized,
            "spatial" => Self::Spatial,
            "text" => Self::Text,
            _ => panic!("family must be scalar|vector|quantized|spatial|text"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::Vector => "vector",
            Self::Quantized => "quantized",
            Self::Spatial => "spatial",
            Self::Text => "text",
        }
    }
}

fn read_barrier() {
    let mut line = String::new();
    assert_ne!(io::stdin().lock().read_line(&mut line).unwrap(), 0);
}

fn signal(value: &str) {
    println!("{value}");
    io::stdout().flush().unwrap();
}

fn apply_updated_transaction(db: &mut Database) {
    let people = CollectionId(1);
    assert_eq!(
        db.put(people, "p0", &person_document("p0", State::Updated))
            .unwrap(),
        entity(1, 1)
    );
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 1),
        EdgeTypeId(1),
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
        EdgeTypeId(1),
        entity(1, 4),
        &json!({"inserted":true}),
    )
    .unwrap();
}

fn open_and_check(dir: &Path, state: State) {
    let db = Database::open_snapshot(dir, cfg()).unwrap();
    check_database(&db, Profile::All, state);
    drop(db);
    check_raw_vector_sidecars(dir, state);
}

fn seed_crash(dir: &Path) {
    generate(Profile::All, dir, "wal-pending");
    open_and_check(dir, State::Original);
}

fn apply_commit(dir: &Path) {
    let mut db = Database::open(dir, cfg()).unwrap();
    check_database(&db, Profile::All, State::Original);
    apply_updated_transaction(&mut db);
    db.commit().unwrap();
    check_database(&db, Profile::All, State::Updated);
    drop(db);
    check_raw_vector_sidecars(dir, State::Updated);
}

fn commit_child(dir: &Path) {
    let mut db = Database::open(dir, cfg()).unwrap();
    check_database(&db, Profile::All, State::Original);
    apply_updated_transaction(&mut db);
    signal("MUTATIONS_READY");
    read_barrier();
    signal("COMMIT_STARTING");
    db.commit().unwrap();
    signal("COMMIT_RETURNED");
    read_barrier();
}

fn hold_snapshot(dir: &Path, state: State, lifecycle: Option<(IndexId, IndexState)>) {
    let db = Database::open_snapshot(dir, cfg()).unwrap();
    check_database(&db, Profile::All, state);
    if let Some((id, expected)) = &lifecycle {
        assert_eq!(db.index_info(*id).unwrap().state, *expected);
    }
    signal("SNAPSHOT_READY");
    read_barrier();
    check_database(&db, Profile::All, state);
    if let Some((id, expected)) = lifecycle {
        assert_eq!(db.index_info(id).unwrap().state, expected);
    }
    signal("SNAPSHOT_STILL_EXACT");
    read_barrier();
}

fn create_duplicate(db: &mut Database, family: LifecycleFamily) -> IndexId {
    match family {
        LifecycleFamily::Scalar => db
            .create_scalar_index(CollectionId(1), "crash_scalar", "age", false)
            .unwrap(),
        LifecycleFamily::Vector => db
            .create_exact_vector_index(CollectionId(1), "crash_vector", "embedding")
            .unwrap(),
        LifecycleFamily::Quantized => db
            .create_quantized_vector_index(CollectionId(1), "crash_quantized", "embedding")
            .unwrap(),
        LifecycleFamily::Spatial => db
            .create_point_index(CollectionId(1), "crash_spatial", "position")
            .unwrap(),
        LifecycleFamily::Text => db
            .create_text_index(CollectionId(1), "crash_text", "body")
            .unwrap(),
    }
}

fn prepare_lifecycle(dir: &Path, family: LifecycleFamily, mode: &str) {
    let mut db = Database::open(dir, cfg()).unwrap();
    check_database(&db, Profile::All, State::Original);
    let id = create_duplicate(&mut db, family);
    db.commit().unwrap();
    match mode {
        "build" => assert!(matches!(
            db.index_info(id).unwrap().state,
            IndexState::Building { after: 0 }
        )),
        "drop" => {
            while !db.build_index_step(id, 1).unwrap() {
                db.commit().unwrap();
            }
            db.commit().unwrap();
            db.begin_drop_index(id).unwrap();
            db.commit().unwrap();
            assert_eq!(db.index_info(id).unwrap().state, IndexState::Dropping);
        }
        _ => panic!("lifecycle mode must be build|drop"),
    }
    println!(
        "{}",
        json!({"index":id.0,"family":family.name(),"mode":mode})
    );
}

fn lifecycle_child(dir: &Path, id: IndexId, mode: &str) {
    let mut db = Database::open(dir, cfg()).unwrap();
    signal("STEP_READY");
    read_barrier();
    let done = match mode {
        "build" => db.build_index_step(id, 1).unwrap(),
        "drop" => db.drop_index_step(id, 1).unwrap(),
        _ => panic!("lifecycle mode must be build|drop"),
    };
    signal(&format!("STEP_MUTATED done={done}"));
    read_barrier();
    signal("STEP_COMMIT_STARTING");
    db.commit().unwrap();
    signal(&format!("STEP_COMMITTED done={done}"));
    read_barrier();
}

fn lifecycle_step_commit(dir: &Path, id: IndexId, mode: &str) {
    let mut db = Database::open(dir, cfg()).unwrap();
    let done = match mode {
        "build" => db.build_index_step(id, 1).unwrap(),
        "drop" => db.drop_index_step(id, 1).unwrap(),
        _ => panic!("lifecycle mode must be build|drop"),
    };
    db.commit().unwrap();
    println!("{}", json!({"committed":true,"done":done}));
}

fn ordered_key(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    let mut key = vec![0x80 + (8 - start) as u8];
    key.extend_from_slice(&bytes[start..]);
    key
}

fn vector_sidecar_key(entity: EntityId) -> Vec<u8> {
    let mut key = vec![0x60];
    key.extend(ordered_key(entity.collection.0.into()));
    key.extend(ordered_key(entity.sequence));
    // Dense layouts reserve ordinal zero for the external key. In the fixture
    // layout embedding follows age and name, so its stable physical ordinal is 3.
    key.extend(ordered_key(3));
    key
}

fn encoded_vector(x: f32, y: f32) -> Vec<u8> {
    [x.to_le_bytes(), y.to_le_bytes()].concat()
}

fn check_raw_vector_sidecars(dir: &Path, state: State) {
    let store = PageWalStore::open_snapshot(dir, CACHE_BYTES).unwrap();
    let expected = match state {
        State::Original => vec![
            (entity(1, 1), encoded_vector(1.0, 0.0)),
            (entity(1, 2), encoded_vector(1.0, 0.0)),
            (entity(1, 3), encoded_vector(0.0, 1.0)),
        ],
        State::Updated => vec![
            (entity(1, 1), encoded_vector(0.0, 1.0)),
            (entity(1, 2), encoded_vector(1.0, 0.0)),
            (entity(1, 4), encoded_vector(1.0, 0.0)),
        ],
    };
    for (id, bytes) in expected {
        assert_eq!(
            store.get(&vector_sidecar_key(id)).unwrap(),
            Some(bytes),
            "authoritative f32 vector sidecar for {id:?}/{state:?}"
        );
    }
    let absent = match state {
        State::Original => entity(1, 4),
        State::Updated => entity(1, 3),
    };
    assert_eq!(store.get(&vector_sidecar_key(absent)).unwrap(), None);
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[usize::from(byte >> 4)] as char);
        output.push(DIGITS[usize::from(byte & 15)] as char);
    }
    output
}

fn lifecycle_signature(dir: &Path, family: LifecycleFamily, id: IndexId) {
    let db = Database::open_snapshot(dir, cfg()).unwrap();
    let state = db
        .list_indexes(CollectionId(1))
        .unwrap()
        .into_iter()
        .find(|index| index.id == id)
        .map(|index| format!("{:?}", index.state))
        .unwrap_or_else(|| "Absent".into());
    drop(db);

    let tags: &[u8] = match family {
        LifecycleFamily::Scalar => &[0x70],
        LifecycleFamily::Vector => &[0x73],
        LifecycleFamily::Quantized => &[0x79],
        LifecycleFamily::Spatial => &[0x74],
        LifecycleFamily::Text => &[0x75, 0x76, 0x77, 0x78],
    };
    let store = PageWalStore::open_snapshot(dir, CACHE_BYTES).unwrap();
    let mut entries = Vec::new();
    for &tag in tags {
        let mut prefix = vec![tag];
        prefix.extend(ordered_key(id.0));
        for row in store.range(&prefix).unwrap() {
            let (key, value) = row.unwrap();
            if !key.starts_with(&prefix) {
                break;
            }
            entries.push(json!([hex(&key), hex(&value)]));
        }
    }
    println!("{}", json!({"state":state,"entries":entries}));
}

fn assert_duplicate(db: &Database, family: LifecycleFamily, id: IndexId) {
    match family {
        LifecycleFamily::Scalar => assert_eq!(
            db.query_scalar(
                id,
                ScalarPredicate::Range {
                    lower: None,
                    upper: None
                },
                16
            )
            .unwrap(),
            db.query_scalar(
                IndexId(1),
                ScalarPredicate::Range {
                    lower: None,
                    upper: None
                },
                16
            )
            .unwrap()
        ),
        LifecycleFamily::Vector => {
            let query = |index| {
                db.query_exact_vector(
                    index,
                    &[1.0, 0.0],
                    VectorMetric::SquaredL2,
                    8,
                    VectorCandidates::All,
                    8,
                    || false,
                )
                .unwrap()
            };
            assert_eq!(query(id), query(IndexId(2)));
        }
        LifecycleFamily::Quantized => {
            let query = |index| {
                db.query_quantized_vector(
                    index,
                    &[0.0, 1.0],
                    VectorMetric::SquaredL2,
                    1,
                    1,
                    QuantizedVectorCandidates::All,
                    8,
                    || false,
                )
                .unwrap()
            };
            assert_eq!(query(id), query(IndexId(3)));
        }
        LifecycleFamily::Spatial => {
            let query = |index| {
                db.query_point_bbox(
                    index,
                    Bounds::new(-0.1, 1.1, -0.1, 0.1).unwrap(),
                    8,
                    SpatialCandidates::All,
                    8,
                    || false,
                )
                .unwrap()
            };
            assert_eq!(query(id), query(IndexId(4)));
        }
        LifecycleFamily::Text => {
            let query = |index| {
                db.query_text(
                    index,
                    "rust database",
                    TextMatch::Any,
                    8,
                    TextCandidates::All,
                    32,
                    || false,
                )
                .unwrap()
            };
            assert_eq!(query(id), query(IndexId(5)));
        }
    }
}

fn inspect_and_resume(dir: &Path, family: LifecycleFamily, mode: &str, id: IndexId) {
    let snapshot = Database::open_snapshot(dir, cfg()).unwrap();
    check_database(&snapshot, Profile::All, State::Original);
    let observed = snapshot
        .list_indexes(CollectionId(1))
        .unwrap()
        .into_iter()
        .find(|index| index.id == id)
        .map(|index| format!("{:?}", index.state))
        .unwrap_or_else(|| "Absent".into());
    drop(snapshot);

    let mut db = Database::open(dir, cfg()).unwrap();
    match mode {
        "build" => {
            while !db.build_index_step(id, 1).unwrap() {
                db.commit().unwrap();
            }
            db.commit().unwrap();
            assert_eq!(db.index_info(id).unwrap().state, IndexState::Ready);
            assert_duplicate(&db, family, id);
        }
        "drop" => {
            while db
                .list_indexes(CollectionId(1))
                .unwrap()
                .iter()
                .any(|index| index.id == id)
            {
                let done = db.drop_index_step(id, 1).unwrap();
                db.commit().unwrap();
                if done {
                    break;
                }
            }
            assert!(!db
                .list_indexes(CollectionId(1))
                .unwrap()
                .iter()
                .any(|index| index.id == id));
        }
        _ => panic!("lifecycle mode must be build|drop"),
    }
    check_database(&db, Profile::All, State::Original);
    println!(
        "{}",
        json!({"observed":observed,"resumed":true,"family":family.name(),"mode":mode})
    );
}

fn checkpoint_crash(dir: &Path, stage: u8) {
    let mut store = PageWalStore::open(dir, false, CACHE_BYTES).unwrap();
    let _ = store.test_checkpoint_crash(stage).unwrap();
    panic!("checkpoint crash stage did not terminate process");
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--version" => println!(
            "{}",
            json!({
                "harness":"phase2-crash-probe-v2",
                "engine_revision":option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded"),
                "create_compact_cells":create_compact_cells(),
                "compile_features":{
                    "compact-cells":cfg!(feature="compact-cells"),
                    "sqlite-balance":cfg!(feature="sqlite-balance"),
                    "keyspace-append":cfg!(feature="keyspace-append"),
                    "slotref-split":cfg!(feature="slotref-split")
                },
                "deterministic_checkpoint_stages":[1,2,3,4,5,6],
                "logical_features":63,
                "lifecycle_families":["scalar","vector","quantized","spatial","text"],
                "quantized_vector_oracles":["exact-rerank","ef1-code-sensitive-winner"],
                "authoritative_vector_oracle":"exact-f32-sidecar-bytes",
                "commit_stage_hook":false,
                "write_fault_hook_available_to_binary":false
            })
        ),
        [command, dir] if command == "seed" => seed_crash(Path::new(dir)),
        [command, dir] if command == "apply-commit" => apply_commit(Path::new(dir)),
        [command, dir] if command == "commit-child" => commit_child(Path::new(dir)),
        [command, dir, state] if command == "verify" => {
            open_and_check(Path::new(dir), parse_state(state));
            println!("{}", json!({"state":state,"exact":true}));
        }
        [command, dir] if command == "classify" => {
            let original = std::panic::catch_unwind(|| open_and_check(Path::new(dir), State::Original)).is_ok();
            let updated = std::panic::catch_unwind(|| open_and_check(Path::new(dir), State::Updated)).is_ok();
            assert_ne!(original, updated, "reopen is neither or ambiguously both complete state oracles");
            println!("{}", json!({"state":if original {"original"} else {"updated"},"exact":true}));
        }
        [command, dir, state] if command == "hold-snapshot" => {
            hold_snapshot(Path::new(dir), parse_state(state), None)
        }
        [command, dir, state, index, lifecycle_state] if command == "hold-snapshot" => {
            let expected = match lifecycle_state.as_str() {
                "building" => IndexState::Building { after: 0 },
                "dropping" => IndexState::Dropping,
                _ => panic!("held lifecycle state must be building|dropping"),
            };
            hold_snapshot(
                Path::new(dir),
                parse_state(state),
                Some((IndexId(index.parse().unwrap()), expected)),
            )
        }
        [command, dir, family, mode] if command == "prepare-lifecycle" => {
            prepare_lifecycle(Path::new(dir), LifecycleFamily::parse(family), mode)
        }
        [command, dir, index, mode] if command == "lifecycle-child" => lifecycle_child(
            Path::new(dir),
            IndexId(index.parse().unwrap()),
            mode,
        ),
        [command, dir, index, mode] if command == "lifecycle-step-commit" => {
            lifecycle_step_commit(Path::new(dir), IndexId(index.parse().unwrap()), mode)
        }
        [command, dir, family, index] if command == "lifecycle-signature" => lifecycle_signature(
            Path::new(dir),
            LifecycleFamily::parse(family),
            IndexId(index.parse().unwrap()),
        ),
        [command, dir, family, mode, index] if command == "inspect-resume" => inspect_and_resume(
            Path::new(dir),
            LifecycleFamily::parse(family),
            mode,
            IndexId(index.parse().unwrap()),
        ),
        [command, dir, stage] if command == "checkpoint-crash" => {
            checkpoint_crash(Path::new(dir), stage.parse().unwrap())
        }
        _ => panic!("usage: phase2_crash_probe --version | seed DIR | apply-commit DIR | commit-child DIR | verify DIR original|updated | classify DIR | hold-snapshot DIR STATE [INDEX building|dropping] | prepare-lifecycle DIR FAMILY build|drop | lifecycle-child DIR INDEX build|drop | lifecycle-step-commit DIR INDEX build|drop | lifecycle-signature DIR FAMILY INDEX | inspect-resume DIR FAMILY build|drop INDEX | checkpoint-crash DIR STAGE"),
    }
}
