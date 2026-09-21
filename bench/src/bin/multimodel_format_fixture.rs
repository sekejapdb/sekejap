//! Candidate Phase-2 multi-model compatibility fixture helper.
//!
//! These fixtures are evidence candidates, not a released or frozen format.
//! `generate` requires a new directory. `upgrade` requires an explicit source
//! manifest and a separately copied target whose files cannot alias source.
use sekejap_core::{
    collections::{
        ApproxVectorMethod, BfsRequest, CollectionId, CollectionOptions, Database, Direction,
        EdgeTypeId, EntityId, GraphContextId, IndexFamily, IndexId, IndexState,
        QuantizedVectorCandidates, ScalarPredicate, SpatialCandidates, TextCandidates, TextMatch,
        VectorCandidates, VectorMetric, SUPPORTED_LOGICAL_FEATURES,
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
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

const CACHE_BYTES: usize = 1 << 20;
const MANIFEST: &str = "MULTIMODEL_FIXTURE.json";
const FORMAT: &str = "e4-phase2-multimodel-candidate-v2";
const PHYSICAL_FEATURES_OFFSET: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Original,
    Updated,
    Roundtrip,
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
        ("p0", State::Roundtrip) => json!({
            "age":15,
            "name":"Alpha Roundtrip",
            "embedding":[1.0,0.0],
            "position":{"type":"Point","coordinates":[0.0,0.0]},
            "body":"rust database",
            "profile":{"numbers":[1,2,3],"nested":{"enabled":true,"roundtrip":true}}
        }),
        ("p3", State::Roundtrip) => person_document("p3", State::Updated),
        ("p4", State::Roundtrip) => json!({
            "age":30,
            "name":"Epsilon",
            "embedding":[0.0,1.0],
            "position":{"type":"Point","coordinates":[1.0,0.0]},
            "body":"search İstanbul",
            "profile":{"numbers":[10,11,12]}
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
        State::Roundtrip => vec![
            (entity(1, 1), "p0", person_document("p0", state)),
            (entity(1, 4), "p3", person_document("p3", state)),
            (entity(1, 5), "p4", person_document("p4", state)),
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
    assert_eq!(
        indexes.len(),
        1 + usize::from(profile.vector())
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
        State::Roundtrip => (
            vec![entity(1, 1), entity(1, 5), entity(1, 4)],
            vec![entity(1, 5)],
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
        State::Roundtrip => vec![
            (entity(1, 1), 0.0),
            (entity(1, 4), 0.0),
            (entity(1, 5), 2.0),
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
        State::Roundtrip => vec![
            (entity(1, 1), 0.0),
            (entity(1, 4), 0.0),
            (entity(1, 5), 2.0),
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
    for (hit, (id, distance)) in result.hits.iter().zip(expected) {
        assert_eq!(hit.id, id);
        assert!((hit.distance - distance).abs() <= 1e-15);
    }
}

fn check_spatial(db: &Database, profile: Profile, state: State) {
    if !profile.spatial() {
        return;
    }
    let index = spatial_index(profile).unwrap();
    let expected = match state {
        State::Original => vec![entity(1, 1), entity(1, 2)],
        State::Updated => vec![entity(1, 2), entity(1, 4)],
        State::Roundtrip => vec![entity(1, 1), entity(1, 4), entity(1, 5)],
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
        State::Roundtrip => (
            vec![
                (entity(1, 1), 1.047096693003158),
                (entity(1, 4), 0.7803833844080139),
            ],
            vec![
                (entity(1, 4), 1.6285466842458854),
                (entity(1, 5), 1.0925692944940748),
            ],
        ),
    };
    assert_text_hits(db, index, "rust database", &ordinary);
    assert_text_hits(db, index, "İSTANBUL café 北京", &unicode);
}

fn neighbors(db: &Database, id: EntityId) -> Vec<EntityId> {
    db.neighbors(sekejap_core::collections::NeighborRequest {
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
        .neighbors(sekejap_core::collections::NeighborRequest {
            entity: entity(1, 1),
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(EdgeTypeId(1)),
            limit: 16,
        })
        .unwrap();
    assert_eq!(p0_knows.len(), 1);
    assert_eq!(
        p0_knows[0].key.destination,
        if state == State::Roundtrip {
            entity(1, 5)
        } else {
            entity(1, 2)
        }
    );
    assert_eq!(
        p0_knows[0].properties,
        match state {
            State::Original => json!({"since":1}),
            State::Updated => json!({"since":7,"updated":true}),
            State::Roundtrip => json!({"roundtrip":true}),
        }
    );
    let membership = db
        .neighbors(sekejap_core::collections::NeighborRequest {
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
        neighbors(
            db,
            if state == State::Roundtrip {
                entity(1, 5)
            } else {
                entity(1, 2)
            }
        ),
        match state {
            State::Original => vec![entity(1, 3)],
            State::Updated => vec![entity(1, 4)],
            State::Roundtrip => vec![entity(1, 4)],
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
            edge_where: &[],
            node_where: &[],
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
            State::Roundtrip => vec![(entity(1, 5), 1), (entity(1, 4), 2)],
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
            "original_examined":3,
            "updated_examined":3,
            "original_exact_rerank_squared_l2":[[[1,1],0.0],[[1,2],0.0],[[1,3],2.0]],
            "updated_exact_rerank_squared_l2":[[[1,2],0.0],[[1,4],0.0],[[1,1],2.0]]
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
        "generator":"bench/src/bin/multimodel_format_fixture.rs",
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

fn validate_manifest(path: &Path) -> (Vec<u8>, Profile, [u64; 2]) {
    let bytes = fs::read(path).expect("fixture manifest");
    let manifest: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(manifest["format"], FORMAT);
    assert_eq!(manifest["status"], "candidate-not-released-or-frozen");
    let profile = Profile::parse(manifest["profile"].as_str().unwrap());
    assert_eq!(
        manifest["required_logical_features"],
        profile.logical_features()
    );
    assert_eq!(manifest["oracle"], manifest_oracle(profile));
    let physical: [u64; 2] = serde_json::from_value(manifest["physical_features"].clone()).unwrap();
    (bytes, profile, physical)
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

fn verify(dir: &Path, state: State) {
    let (_, profile, expected_physical) = validate_manifest(&dir.join(MANIFEST));
    assert_eq!(physical_features(dir), expected_physical);
    let db = Database::open_snapshot(dir, cfg()).unwrap();
    check_database(&db, profile, state);
    drop(db);
    assert_required_features(dir, profile);
}

fn upgrade(source_manifest: &Path, copied: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    let (source_manifest, copied) = guarded_upgrade_paths(source_manifest, copied);
    let (source_bytes, profile, expected_physical) = validate_manifest(&source_manifest);
    assert_eq!(
        fs::read(copied.join(MANIFEST)).unwrap(),
        source_bytes,
        "copied target manifest differs from explicit source"
    );
    assert_eq!(physical_features(&copied), expected_physical);
    verify(&copied, State::Original);
    let mut db = Database::open(&copied, cfg()).unwrap();
    let old = Database::open_snapshot(&copied, cfg()).unwrap();
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
    db.commit().unwrap();
    check_database(&db, profile, State::Updated);
    check_database(&old, profile, State::Original);
    drop(old);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    check_boundary(&copied, boundary);
    assert_required_features(&copied, profile);
    assert_eq!(physical_features(&copied), expected_physical);
}

fn continue_upgrade(source_manifest: &Path, copied: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    let (source_manifest, copied) = guarded_upgrade_paths(source_manifest, copied);
    let (source_bytes, profile, expected_physical) = validate_manifest(&source_manifest);
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
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 1),
        EdgeTypeId(1),
        entity(1, 5),
        &json!({"roundtrip":true}),
    )
    .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        entity(1, 5),
        EdgeTypeId(1),
        entity(1, 4),
        &json!({"bridge":"old-writer"}),
    )
    .unwrap();
    db.commit().unwrap();
    check_database(&db, profile, State::Roundtrip);
    check_database(&old, profile, State::Updated);
    drop(old);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    check_boundary(&copied, boundary);
    assert_required_features(&copied, profile);
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
    let (source_bytes, profile, expected_physical) = validate_manifest(&source_manifest);
    assert_eq!(fs::read(copied.join(MANIFEST)).unwrap(), source_bytes);
    assert_eq!(physical_features(&copied), expected_physical);
    verify(&copied, state);
    let db = Database::open(&copied, cfg()).unwrap();
    check_database(&db, profile, state);
    drop(db);
    assert_required_features(&copied, profile);
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
                "harness":"multimodel-format-fixture-v2",
                "rollback_cycle_version":1,
                "status":"candidate-not-released-or-frozen",
                "engine_revision":option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded"),
                "create_compact_cells":create_compact_cells(),
                "supported_logical_features":SUPPORTED_LOGICAL_FEATURES,
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
        [command, profile, dir, boundary] if command == "generate" => {
            let profile = Profile::parse(profile);
            generate(profile, Path::new(dir), boundary);
            println!(
                "{}",
                json!({"result":"PASS","command":"generate","profile":profile.name(),"database":dir,"boundary":boundary})
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
            "usage:\n  multimodel_format_fixture generate vector|quantized|spatial|text|all NEW_DIR checkpointed|wal-pending\n  multimodel_format_fixture verify EXISTING_DIR original|updated|roundtrip\n  multimodel_format_fixture upgrade SOURCE_MANIFEST COPIED_DIR checkpointed|wal-pending --confirm-copy\n  multimodel_format_fixture continue-upgrade SOURCE_MANIFEST COPIED_DIR checkpointed|wal-pending --confirm-copy\n  multimodel_format_fixture verify-writer SOURCE_MANIFEST COPIED_DIR roundtrip --confirm-copy"
        ),
    }
}
