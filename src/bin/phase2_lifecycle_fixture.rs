//! Candidate graph-independent index lifecycle compatibility fixtures.
//!
//! These fixtures are not a released or frozen format. `generate` requires a
//! new directory. `upgrade` mutates only a separately copied target after
//! checking that no source file is aliased by path, symlink, or hard link.
use e4_prototype::{
    Kind,
    collections::{
        ApproxVectorMethod, CollectionId, CollectionOptions, Database, EntityId, Error,
        create_index_trees, IndexFamily, IndexId, IndexState, IndexTree,
        QuantizedVectorCandidates, ScalarPredicate, SpatialCandidates,
        SUPPORTED_LOGICAL_FEATURES, TextCandidates, TextMatch, VectorCandidates, VectorMetric,
    },
    pagewal::{PageWalStore, create_compact_cells},
    spatial_math::Bounds,
};
use kernel::{
    io::IoMode,
    page::{PAGE_SIZE, PageRef},
    store::{Config, SyncMode},
};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

const CACHE_BYTES: usize = 1 << 20;
const MANIFEST: &str = "PHASE2_LIFECYCLE_FIXTURE.json";
const FORMAT: &str = "e4-phase2-index-lifecycle-candidate-v1";
const PHYSICAL_FEATURES_OFFSET: usize = 48;
const ROWS: usize = 10;
/// The collection-header bit that says this database holds at least one index
/// with its OWN B-tree. Scalar and spatial indexes are created that way while
/// the handle's create-index-trees switch is on, so the corpus they generate
/// requires this bit on top of its family bit, and a binary that predates
/// per-index trees must refuse the file whole rather than read past a
/// descriptor it cannot parse.
const INDEX_TREE_FEATURE: u64 = 0x80;
const INDEX: IndexId = IndexId(1);
const PEOPLE: CollectionId = CollectionId(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Scalar,
    ExactVector,
    Spatial,
    Text,
    Quantized,
}

impl Family {
    fn parse(value: &str) -> Self {
        match value {
            "scalar" => Self::Scalar,
            "exact-vector" => Self::ExactVector,
            "spatial" => Self::Spatial,
            "text" => Self::Text,
            "quantized" => Self::Quantized,
            _ => panic!("family must be scalar|exact-vector|spatial|text|quantized"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::ExactVector => "exact-vector",
            Self::Spatial => "spatial",
            Self::Text => "text",
            Self::Quantized => "quantized",
        }
    }

    /// The two families whose indexes get a B-tree of their own when the
    /// handle's create-index-trees switch is on. Every other family keeps its
    /// entries in the primary tree at descriptor version 1, whatever the
    /// switch says.
    fn owns_tree(self) -> bool {
        matches!(self, Self::Scalar | Self::Spatial)
    }

    /// The descriptor version an index of this family is CREATED at, under the
    /// switch this binary creates databases with. The fixture asserts what it
    /// actually wrote, so this is read from the engine's create-time default
    /// rather than pinned to either layout.
    fn encoding_version(self) -> u16 {
        if self.owns_tree() && create_index_trees() {
            2
        } else {
            1
        }
    }

    fn logical_features(self) -> u64 {
        let family = 1 | match self {
            Self::Scalar => 0,
            Self::ExactVector => 4,
            Self::Spatial => 8,
            Self::Text => 16,
            Self::Quantized => 32,
        };
        if self.encoding_version() == 2 {
            family | INDEX_TREE_FEATURE
        } else {
            family
        }
    }

    fn index_family(self) -> IndexFamily {
        match self {
            Self::Scalar => IndexFamily::Scalar,
            Self::ExactVector => IndexFamily::ExactVector,
            Self::Spatial => IndexFamily::SpatialPoint,
            Self::Text => IndexFamily::Text,
            Self::Quantized => IndexFamily::QuantizedVector,
        }
    }

    fn index_name(self) -> &'static str {
        match self {
            Self::Scalar => "age_idx",
            Self::ExactVector => "embedding_exact",
            Self::Spatial => "position_point",
            Self::Text => "body_text",
            Self::Quantized => "embedding_int8",
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::Scalar => "age",
            Self::ExactVector | Self::Quantized => "embedding",
            Self::Spatial => "position",
            Self::Text => "body",
        }
    }

    fn kind(self) -> Kind {
        match self {
            Self::Scalar => Kind::Int,
            Self::ExactVector | Self::Quantized => Kind::Vector(2),
            Self::Spatial => Kind::Point,
            Self::Text => Kind::Text,
        }
    }

    fn derived_tags(self) -> &'static [u8] {
        match self {
            Self::Scalar => &[0x70],
            Self::ExactVector => &[0x73],
            Self::Spatial => &[0x74],
            Self::Text => &[0x75, 0x76, 0x77, 0x78],
            Self::Quantized => &[0x79],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifecycle {
    Ready,
    Building,
    Dropping,
    PostDrop,
}

impl Lifecycle {
    fn parse(value: &str) -> Self {
        match value {
            "ready" => Self::Ready,
            "building" => Self::Building,
            "dropping" => Self::Dropping,
            "post-drop" => Self::PostDrop,
            _ => panic!("lifecycle must be ready|building|dropping|post-drop"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Building => "building",
            Self::Dropping => "dropping",
            Self::PostDrop => "post-drop",
        }
    }

    fn resolves_ready(self) -> bool {
        matches!(self, Self::Ready | Self::Building)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataState {
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

fn entity(sequence: u64) -> EntityId {
    EntityId {
        collection: PEOPLE,
        sequence,
    }
}

fn vector_for(row: usize, state: DataState) -> [f32; 2] {
    if state != DataState::Original && row == 1 {
        [1.0, 0.0]
    } else if state != DataState::Original && row == 10 {
        [0.0, 1.0]
    } else if state == DataState::Roundtrip && row == 3 {
        [0.5, 0.5]
    } else if state == DataState::Roundtrip && row == 11 {
        [-1.0, 0.0]
    } else if row % 2 == 0 {
        [1.0, 0.0]
    } else {
        [0.0, 1.0]
    }
}

fn document(row: usize, state: DataState) -> Value {
    let first_updated = state != DataState::Original && row == 1;
    let first_inserted = state != DataState::Original && row == 10;
    let second_updated = state == DataState::Roundtrip && row == 3;
    let second_inserted = state == DataState::Roundtrip && row == 11;
    let age = if first_updated {
        99
    } else if first_inserted {
        30
    } else if second_updated {
        77
    } else if second_inserted {
        31
    } else {
        20 + row as i64
    };
    let (longitude, latitude) = if first_updated {
        (5.0, 5.0)
    } else if first_inserted {
        (0.25, 0.0)
    } else if second_updated {
        (5.0, -5.0)
    } else if second_inserted {
        (0.1, 0.0)
    } else {
        (row as f64 / 10.0, 0.0)
    };
    let body = if first_updated {
        "alpha updated".to_owned()
    } else if first_inserted {
        "alpha inserted".to_owned()
    } else if second_updated {
        "alpha revised".to_owned()
    } else if second_inserted {
        "alpha roundtrip".to_owned()
    } else if row % 2 == 0 {
        "alpha common".to_owned()
    } else {
        "beta common".to_owned()
    };
    json!({
        "age":age,
        "name":format!("Person {row:02}"),
        "embedding":vector_for(row, state),
        "position":{"type":"Point","coordinates":[longitude,latitude]},
        "body":body,
        "profile":{"ordinal":row as u64,"exact":u64::MAX,"nested":{"ok":true}}
    })
}

fn expected_rows(state: DataState) -> Vec<(EntityId, String, Value)> {
    match state {
        DataState::Original => (0..ROWS)
            .map(|row| {
                (
                    entity(row as u64 + 1),
                    format!("p{row}"),
                    document(row, state),
                )
            })
            .collect(),
        DataState::Updated => (0..ROWS)
            .filter(|row| *row != 2)
            .chain(std::iter::once(10))
            .map(|row| {
                let sequence = if row == 10 { 11 } else { row as u64 + 1 };
                (entity(sequence), format!("p{row}"), document(row, state))
            })
            .collect(),
        DataState::Roundtrip => (0..ROWS)
            .filter(|row| *row != 0 && *row != 2)
            .chain([10, 11])
            .map(|row| {
                let sequence = match row {
                    10 => 11,
                    11 => 12,
                    _ => row as u64 + 1,
                };
                (entity(sequence), format!("p{row}"), document(row, state))
            })
            .collect(),
    }
}

fn check_rows(db: &Database, state: DataState) {
    let expected = expected_rows(state);
    let mut oracle = BTreeMap::new();
    for (id, key, document) in expected {
        let row = db.get(PEOPLE, &key).unwrap().expect("expected entity");
        assert_eq!(row.id, id, "stable entity ID for {key}");
        assert_eq!(row.key, key);
        assert_eq!(row.document, document, "typed payload for {key}");
        assert_eq!(db.get_by_id(id).unwrap(), Some(row.clone()));
        assert!(oracle.insert(id, (key, document)).is_none());
    }
    let actual = db
        .scan(PEOPLE, None)
        .unwrap()
        .map(|row| {
            let row = row.unwrap();
            (row.id, (row.key, row.document))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(actual, oracle);
    if state != DataState::Original {
        assert!(db.get(PEOPLE, "p2").unwrap().is_none());
    }
    if state == DataState::Roundtrip {
        assert!(db.get(PEOPLE, "p0").unwrap().is_none());
    }
}

fn expected_index_state(lifecycle: Lifecycle, upgraded: bool) -> Option<IndexState> {
    if upgraded {
        lifecycle.resolves_ready().then_some(IndexState::Ready)
    } else {
        match lifecycle {
            Lifecycle::Ready => Some(IndexState::Ready),
            Lifecycle::Building => Some(IndexState::Building { after: 3 }),
            Lifecycle::Dropping => Some(IndexState::Dropping),
            Lifecycle::PostDrop => None,
        }
    }
}

fn check_catalog(db: &Database, family: Family, lifecycle: Lifecycle, upgraded: bool) {
    let expected = expected_index_state(lifecycle, upgraded);
    let indexes = db.list_indexes(PEOPLE).unwrap();
    if let Some(state) = expected {
        assert_eq!(indexes.len(), 1);
        let index = &indexes[0];
        assert_eq!(index.id, INDEX);
        assert_eq!(index.collection, PEOPLE);
        assert_eq!(index.name, family.index_name());
        assert_eq!(index.field, family.field());
        assert_eq!(index.family, family.index_family());
        assert_eq!(index.kind, family.kind());
        assert!(!index.unique);
        assert_eq!(index.state, state);
        assert_eq!(index.encoding_version, family.encoding_version());
        assert_eq!(index.tree.is_some(), index.encoding_version == 2);
        assert_eq!(db.index_info(INDEX).unwrap(), *index);
    } else {
        assert!(indexes.is_empty());
        assert!(matches!(
            db.index_info(INDEX),
            Err(Error::NotFound("index"))
        ));
    }
}

fn independent_vector_hits(state: DataState) -> Vec<(EntityId, f64)> {
    let mut hits = expected_rows(state)
        .into_iter()
        .map(|(id, key, _)| {
            let row = key.strip_prefix('p').unwrap().parse::<usize>().unwrap();
            let vector = vector_for(row, state);
            let dx = f64::from(vector[0]) - 1.0;
            let dy = f64::from(vector[1]);
            (id, dx * dx + dy * dy)
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| left.1.total_cmp(&right.1).then(left.0.cmp(&right.0)));
    hits
}

fn check_ready_query(db: &Database, family: Family, state: DataState) {
    match family {
        Family::Scalar => {
            let mut all = expected_rows(state)
                .into_iter()
                .map(|(id, _, document)| (document["age"].as_i64().unwrap(), id))
                .collect::<Vec<_>>();
            all.sort();
            assert_eq!(
                db.query_scalar(
                    INDEX,
                    ScalarPredicate::Range {
                        lower: None,
                        upper: None,
                    },
                    32,
                )
                .unwrap(),
                all.into_iter().map(|row| row.1).collect::<Vec<_>>()
            );
            let (value, expected) = match state {
                DataState::Original => (json!(21), vec![entity(2)]),
                DataState::Updated => (json!(30), vec![entity(11)]),
                DataState::Roundtrip => (json!(31), vec![entity(12)]),
            };
            assert_eq!(
                db.query_scalar(INDEX, ScalarPredicate::Eq(value), 32)
                    .unwrap(),
                expected
            );
        }
        Family::ExactVector => {
            let hits = db
                .query_exact_vector(
                    INDEX,
                    &[1.0, 0.0],
                    VectorMetric::SquaredL2,
                    32,
                    VectorCandidates::All,
                    32,
                    || false,
                )
                .unwrap();
            let expected = independent_vector_hits(state);
            assert_eq!(hits.len(), expected.len());
            for (hit, &(id, distance)) in hits.iter().zip(&expected) {
                assert_eq!((hit.id, hit.distance.to_bits()), (id, distance.to_bits()));
            }
        }
        Family::Quantized => {
            let result = db
                .query_quantized_vector(
                    INDEX,
                    &[1.0, 0.0],
                    VectorMetric::SquaredL2,
                    32,
                    32,
                    QuantizedVectorCandidates::All,
                    32,
                    || false,
                )
                .unwrap();
            let expected = independent_vector_hits(state);
            assert_eq!(result.method, ApproxVectorMethod::SymmetricInt8ScanV1);
            assert_eq!(result.examined, expected.len());
            assert_eq!(result.reranked, expected.len());
            assert_eq!(result.hits.len(), expected.len());
            for (hit, &(id, distance)) in result.hits.iter().zip(&expected) {
                assert_eq!((hit.id, hit.distance.to_bits()), (id, distance.to_bits()));
            }
        }
        Family::Spatial => {
            let expected = match state {
                DataState::Original => vec![entity(1), entity(2), entity(3), entity(4)],
                DataState::Updated => vec![entity(1), entity(4), entity(11)],
                DataState::Roundtrip => vec![entity(11), entity(12)],
            };
            assert_eq!(
                db.query_point_bbox(
                    INDEX,
                    Bounds::new(-0.01, 0.31, -0.01, 0.01).unwrap(),
                    32,
                    SpatialCandidates::All,
                    32,
                    || false,
                )
                .unwrap(),
                expected
            );
        }
        Family::Text => {
            let expected_ids = match state {
                DataState::Original => vec![entity(1), entity(3), entity(5), entity(7), entity(9)],
                DataState::Updated => {
                    vec![
                        entity(1),
                        entity(2),
                        entity(5),
                        entity(7),
                        entity(9),
                        entity(11),
                    ]
                }
                DataState::Roundtrip => {
                    vec![
                        entity(2),
                        entity(4),
                        entity(5),
                        entity(7),
                        entity(9),
                        entity(11),
                        entity(12),
                    ]
                }
            };
            let documents = 10.0f64;
            let df = expected_ids.len() as f64;
            let expected_score = (1.0 + (documents - df + 0.5) / (df + 0.5)).ln();
            let hits = db
                .query_text(
                    INDEX,
                    "alpha",
                    TextMatch::Any,
                    32,
                    TextCandidates::All,
                    64,
                    || false,
                )
                .unwrap();
            assert_eq!(
                hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
                expected_ids
            );
            for hit in hits {
                assert!((hit.score - expected_score).abs() <= 1e-14);
            }
        }
    }
}

fn ordered(number: u64) -> Vec<u8> {
    let bytes = number.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

fn raw_catalog_keys(family: Family) -> Vec<Vec<u8>> {
    let mut registry = vec![4];
    registry.extend(ordered(INDEX.0));
    let mut mapping = vec![5];
    mapping.extend(ordered(u64::from(PEOPLE.0)));
    mapping.extend(ordered(INDEX.0));
    let mut name = vec![0x11];
    name.extend(ordered(u64::from(PEOPLE.0)));
    name.extend(family.index_name().as_bytes());
    let mut keys = vec![registry, mapping, name];
    for copy in 0..3 {
        let mut descriptor = vec![3, copy];
        descriptor.extend(ordered(INDEX.0));
        keys.push(descriptor);
    }
    keys
}

fn assert_catalog_presence(dir: &Path, family: Family, present: bool) {
    let store = PageWalStore::open_snapshot(dir, CACHE_BYTES).unwrap();
    for key in raw_catalog_keys(family) {
        assert_eq!(
            store.get(&key).unwrap().is_some(),
            present,
            "catalog key {key:?}"
        );
    }
}

fn derived_entry_count(dir: &Path, family: Family) -> usize {
    derived_namespace(dir, family).len()
}

/// The B-tree this fixture's index keeps its entries in, or `None` when the
/// entries are records of the primary tree -- a version-1 descriptor, a family
/// that never owns a tree, or an index already dropped. The descriptor's own
/// bytes are checked elsewhere (`check_catalog`, `assert_catalog_presence`);
/// what is wanted here is only which tree to walk, so it is read through the
/// catalog rather than by re-deriving the tail offset of every family's
/// descriptor inside the fixture.
fn index_tree(dir: &Path, family: Family) -> Option<IndexTree> {
    if family.encoding_version() != 2 {
        return None;
    }
    let db = Database::open_snapshot(dir, cfg()).unwrap();
    let tree = match db.index_info(INDEX) {
        Ok(info) => {
            assert_eq!(info.encoding_version, 2);
            info.tree
        }
        Err(Error::NotFound("index")) => None,
        Err(error) => panic!("index descriptor: {error:?}"),
    };
    assert!(tree.is_none_or(|t| t.id >= 2), "per-index tree id 0/1 is reserved");
    tree
}

fn derived_namespace(dir: &Path, family: Family) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let tree = index_tree(dir, family);
    let store = PageWalStore::open_snapshot(dir, CACHE_BYTES).unwrap();
    let mut entries = BTreeMap::new();
    for &tag in family.derived_tags() {
        let mut prefix = vec![tag];
        prefix.extend(ordered(INDEX.0));
        // An index that owns a tree keeps its entries there and NOTHING under
        // these tags in the primary tree; one that does not owns nothing but
        // the primary tree. Both halves are checked, so a build that wrote to
        // the wrong tree fails here rather than passing by counting twice.
        let rows = match tree {
            Some(t) => {
                assert!(
                    store
                        .range(&prefix)
                        .unwrap()
                        .next()
                        .is_none_or(|row| !row.unwrap().0.starts_with(&prefix)),
                    "entries of an owned index tree leaked into the primary tree"
                );
                store.tree_range(t.id, t.root, &prefix).unwrap()
            }
            None => Some(store.range(&prefix).unwrap()),
        };
        let Some(rows) = rows else { continue };
        for row in rows {
            let (key, value) = row.unwrap();
            if !key.starts_with(&prefix) {
                break;
            }
            assert!(entries.insert(key, value).is_none());
        }
    }
    entries
}

fn indexed_key(tag: u8, sequence: u64) -> Vec<u8> {
    let mut key = vec![tag];
    key.extend(ordered(INDEX.0));
    key.extend(ordered(sequence));
    key
}

fn locator() -> [u8; 6] {
    let mut locator = [0; 6];
    locator[..4].copy_from_slice(&1u32.to_be_bytes());
    // Immutable layouts prepend the internal `__e4_key` Text field. The
    // public embedding field is therefore physical slot 3, not public slot 2.
    locator[4..].copy_from_slice(&3u16.to_be_bytes());
    locator
}

fn vector_sidecar_key(sequence: u64) -> Vec<u8> {
    let mut key = vec![0x60];
    key.extend(ordered(u64::from(PEOPLE.0)));
    key.extend(ordered(sequence));
    key.extend(ordered(3));
    key
}

fn vector_bytes(vector: [f32; 2]) -> Vec<u8> {
    vector
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>()
}

fn expected_building_namespace(family: Family) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut expected = BTreeMap::new();
    match family {
        Family::Scalar => {
            for (sequence, age) in [(1, 20i64), (2, 99), (11, 30)] {
                let mut key = vec![0x70];
                key.extend(ordered(INDEX.0));
                key.push(2);
                key.extend_from_slice(&((age as u64) ^ (1 << 63)).to_be_bytes());
                key.extend(ordered(sequence));
                expected.insert(key, Vec::new());
            }
        }
        Family::ExactVector => {
            for sequence in [1, 2, 11] {
                expected.insert(indexed_key(0x73, sequence), locator().to_vec());
            }
        }
        Family::Quantized => {
            for (sequence, vector) in [(1, [1.0, 0.0]), (2, [1.0, 0.0]), (11, [0.0, 1.0])] {
                let mut value = locator().to_vec();
                value.extend_from_slice(&(1.0f64 / 127.0).to_le_bytes());
                value.extend(vector.map(|lane| (lane * 127.0) as u8));
                expected.insert(indexed_key(0x79, sequence), value);
            }
        }
        Family::Spatial => {
            // Fixed independently calculated 16-bit-grid Hilbert cells.
            for (sequence, longitude, latitude, hilbert) in [
                (1, 0.0f64, 0.0f64, 2_147_483_648u32),
                (2, 5.0, 5.0, 2_149_247_244),
                (11, 0.25, 0.0, 2_147_484_913),
            ] {
                let mut key = vec![0x74];
                key.extend(ordered(INDEX.0));
                key.extend_from_slice(&hilbert.to_be_bytes());
                key.extend(ordered(sequence));
                let mut value = longitude.to_le_bytes().to_vec();
                value.extend_from_slice(&latitude.to_le_bytes());
                expected.insert(key, value);
            }
        }
        Family::Text => {
            let documents = [
                (1, ["alpha", "common"]),
                (2, ["alpha", "updated"]),
                (11, ["alpha", "inserted"]),
            ];
            let mut frequencies = BTreeMap::<&str, u64>::new();
            for (sequence, terms) in documents {
                for term in terms {
                    let mut key = vec![0x75];
                    key.extend(ordered(INDEX.0));
                    key.extend_from_slice(term.as_bytes());
                    key.push(0);
                    key.extend(ordered(sequence));
                    expected.insert(key, 1u32.to_be_bytes().to_vec());
                    *frequencies.entry(term).or_default() += 1;
                }
                expected.insert(indexed_key(0x76, sequence), 2u32.to_be_bytes().to_vec());
            }
            for (term, frequency) in frequencies {
                let mut key = vec![0x77];
                key.extend(ordered(INDEX.0));
                key.extend_from_slice(term.as_bytes());
                key.push(0);
                expected.insert(key, frequency.to_be_bytes().to_vec());
            }
            let mut corpus = 3u64.to_be_bytes().to_vec();
            corpus.extend_from_slice(&6u64.to_be_bytes());
            let mut corpus_key = vec![0x78];
            corpus_key.extend(ordered(INDEX.0));
            expected.insert(corpus_key, corpus);
        }
    }
    expected
}

fn assert_building_after_writes(dir: &Path, family: Family) {
    assert_eq!(
        derived_namespace(dir, family),
        expected_building_namespace(family),
        "BUILDING derived bytes after ordinary update/delete/insert"
    );
    if matches!(family, Family::ExactVector | Family::Quantized) {
        let store = PageWalStore::open_snapshot(dir, CACHE_BYTES).unwrap();
        for (sequence, vector) in [(1, [1.0, 0.0]), (2, [1.0, 0.0]), (11, [0.0, 1.0])] {
            assert_eq!(
                store.get(&vector_sidecar_key(sequence)).unwrap(),
                Some(vector_bytes(vector)),
                "authoritative vector sidecar for sequence {sequence}"
            );
        }
        assert_eq!(store.get(&vector_sidecar_key(3)).unwrap(), None);
    }
}

fn expected_derived_entries(
    family: Family,
    lifecycle: Lifecycle,
    state: DataState,
    upgraded: bool,
) -> usize {
    if upgraded {
        return if lifecycle.resolves_ready() {
            if family == Family::Text {
                match state {
                    DataState::Original => unreachable!("original state is not upgraded"),
                    DataState::Updated => 36,
                    DataState::Roundtrip => 38,
                }
            } else {
                10
            }
        } else {
            0
        };
    }
    match lifecycle {
        Lifecycle::Ready => {
            if family == Family::Text {
                34
            } else {
                10
            }
        }
        Lifecycle::Building => {
            if family == Family::Text {
                13
            } else {
                3
            }
        }
        Lifecycle::Dropping => {
            if family == Family::Text {
                33
            } else {
                9
            }
        }
        Lifecycle::PostDrop => 0,
    }
}

fn assert_required_features(dir: &Path, family: Family, live_index_count: u32) {
    let store = PageWalStore::open_snapshot(dir, CACHE_BYTES).unwrap();
    for copy in 0..3u8 {
        let packet = store.get(&[0, 0, copy]).unwrap().unwrap();
        assert_eq!(&packet[..8], b"E4COLL2\0");
        assert_eq!(u16::from_be_bytes(packet[8..10].try_into().unwrap()), 28);
        assert_eq!(
            crc32c::crc32c(&packet[..packet.len() - 4]),
            u32::from_le_bytes(packet[packet.len() - 4..].try_into().unwrap())
        );
        assert_eq!(
            u64::from_be_bytes(packet[18..26].try_into().unwrap()),
            family.logical_features()
        );
        assert_eq!(u64::from_be_bytes(packet[26..34].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_be_bytes(packet[34..38].try_into().unwrap()),
            live_index_count
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

fn check_database(
    db: &Database,
    family: Family,
    lifecycle: Lifecycle,
    state: DataState,
    upgraded: bool,
) {
    assert_eq!(db.collection("people").unwrap(), Some(PEOPLE));
    assert_eq!(
        db.collection_info(PEOPLE).unwrap().layout.fields,
        vec![
            ("age".into(), Kind::Int),
            ("name".into(), Kind::Text),
            ("embedding".into(), Kind::Vector(2)),
            ("position".into(), Kind::Point),
            ("body".into(), Kind::Text),
            ("profile".into(), Kind::Json),
        ]
    );
    check_rows(db, state);
    check_catalog(db, family, lifecycle, upgraded);
    if expected_index_state(lifecycle, upgraded) == Some(IndexState::Ready) {
        check_ready_query(db, family, state);
    }
}

fn create_index(db: &mut Database, family: Family) -> IndexId {
    match family {
        Family::Scalar => {
            db.create_scalar_index(PEOPLE, family.index_name(), family.field(), false)
        }
        Family::ExactVector => {
            db.create_exact_vector_index(PEOPLE, family.index_name(), family.field())
        }
        Family::Spatial => db.create_point_index(PEOPLE, family.index_name(), family.field()),
        Family::Text => db.create_text_index(PEOPLE, family.index_name(), family.field()),
        Family::Quantized => {
            db.create_quantized_vector_index(PEOPLE, family.index_name(), family.field())
        }
    }
    .unwrap()
}

fn finish_build(db: &mut Database) {
    loop {
        if db.build_index_step(INDEX, 3).unwrap() {
            break;
        }
        db.commit().unwrap();
    }
}

fn finish_drop(db: &mut Database) {
    loop {
        if db.drop_index_step(INDEX, 2).unwrap() {
            break;
        }
        db.commit().unwrap();
    }
}

fn populate(db: &mut Database, family: Family, lifecycle: Lifecycle) {
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
    assert_eq!(people, PEOPLE);
    for row in 0..ROWS {
        assert_eq!(
            db.put(
                PEOPLE,
                &format!("p{row}"),
                &document(row, DataState::Original)
            )
            .unwrap(),
            entity(row as u64 + 1)
        );
    }
    assert_eq!(create_index(db, family), INDEX);
    match lifecycle {
        Lifecycle::Ready => finish_build(db),
        Lifecycle::Building => {
            assert!(!db.build_index_step(INDEX, 3).unwrap());
            assert_eq!(
                db.index_info(INDEX).unwrap().state,
                IndexState::Building { after: 3 }
            );
        }
        Lifecycle::Dropping => {
            finish_build(db);
            db.begin_drop_index(INDEX).unwrap();
            assert!(!db.drop_index_step(INDEX, 1).unwrap());
            assert_eq!(db.index_info(INDEX).unwrap().state, IndexState::Dropping);
        }
        Lifecycle::PostDrop => {
            finish_build(db);
            db.begin_drop_index(INDEX).unwrap();
            finish_drop(db);
        }
    }
}

fn manifest_oracle(family: Family, lifecycle: Lifecycle) -> Value {
    let selected_query = match family {
        Family::Scalar => json!({
            "kind":"scalar",
            "original_all_by_value_then_id":[1,2,3,4,5,6,7,8,9,10],
            "updated_all_by_value_then_id":[1,4,5,6,7,8,9,10,11,2],
            "original_eq":{"value":21,"ids":[2]},
            "updated_eq":{"value":30,"ids":[11]}
        }),
        Family::ExactVector | Family::Quantized => {
            let hits = |state| {
                independent_vector_hits(state)
                    .into_iter()
                    .map(|(id, distance)| json!([id.sequence, distance]))
                    .collect::<Vec<_>>()
            };
            json!({
                "kind":if family==Family::ExactVector{"exact-vector"}else{"quantized-exact-rerank"},
                "query":[1.0,0.0],
                "metric":"squared-l2",
                "original":hits(DataState::Original),
                "updated":hits(DataState::Updated)
            })
        }
        Family::Spatial => json!({
            "kind":"bbox",
            "bounds":[-0.01,0.31,-0.01,0.01],
            "original_ids":[1,2,3,4],
            "updated_ids":[1,4,11]
        }),
        Family::Text => {
            let score = |df: f64| (1.0 + (10.0 - df + 0.5) / (df + 0.5)).ln();
            json!({
                "kind":"text-any-positive-bm25",
                "query":"alpha",
                "original_ids":[1,3,5,7,9],
                "original_score":score(5.0),
                "updated_ids":[1,2,5,7,9,11],
                "updated_score":score(6.0)
            })
        }
    };
    json!({
        "rows":ROWS,
        "original_ids":[1,2,3,4,5,6,7,8,9,10],
        "updated_ids":[1,2,4,5,6,7,8,9,10,11],
        "deleted_external_key":"p2",
        "inserted_external_key":"p10",
        "inserted_id":11,
        "index_id":1,
        "family":family.name(),
        "source_lifecycle":lifecycle.name(),
        "source_building_after":(lifecycle == Lifecycle::Building).then_some(3),
        "source_drop_steps":usize::from(lifecycle == Lifecycle::Dropping),
        "upgraded_lifecycle":if lifecycle.resolves_ready(){"ready"}else{"post-drop"},
        "source_derived_entries":expected_derived_entries(family,lifecycle,DataState::Original,false),
        "upgraded_derived_entries":expected_derived_entries(family,lifecycle,DataState::Updated,true),
        "selected_query":selected_query
    })
}

fn write_manifest(
    dir: &Path,
    family: Family,
    lifecycle: Lifecycle,
    boundary: &str,
    physical: [u64; 2],
) {
    let manifest = json!({
        "format":FORMAT,
        "status":"candidate-not-released-or-frozen",
        "regression_role":"permanent candidate lifecycle corpus for future accepted-version compatibility qualification",
        "current_claim":"same-revision physical-codec cross-build plus preserved older-candidate typed admission",
        "released_baseline_relationship":"Phase 1 remains the actual released baseline; this candidate does not replace it",
        "generator":"src/bin/phase2_lifecycle_fixture.rs",
        "engine_revision":option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded"),
        "family":family.name(),
        "lifecycle":lifecycle.name(),
        "source_generation_boundary":boundary,
        "required_logical_features":family.logical_features(),
        "graph_enabled":false,
        "physical_features":physical,
        "oracle":manifest_oracle(family,lifecycle)
    });
    fs::write(
        dir.join(MANIFEST),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn validate_manifest(path: &Path) -> (Vec<u8>, Family, Lifecycle, [u64; 2]) {
    let bytes = fs::read(path).expect("fixture manifest");
    let manifest: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(manifest["format"], FORMAT);
    assert_eq!(manifest["status"], "candidate-not-released-or-frozen");
    assert_eq!(manifest["graph_enabled"], false);
    let family = Family::parse(manifest["family"].as_str().unwrap());
    let lifecycle = Lifecycle::parse(manifest["lifecycle"].as_str().unwrap());
    assert_eq!(
        manifest["required_logical_features"],
        family.logical_features()
    );
    assert_eq!(manifest["oracle"], manifest_oracle(family, lifecycle));
    let physical: [u64; 2] = serde_json::from_value(manifest["physical_features"].clone()).unwrap();
    (bytes, family, lifecycle, physical)
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
    assert!(!source_manifest.starts_with(&copied));
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
        let name = entry.file_name().into_string().expect("UTF-8 fixture name");
        assert!(
            known_fixture_file(&name),
            "unexpected fixture entry {name:?}"
        );
        let metadata = fs::symlink_metadata(entry.path()).unwrap();
        assert!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "fixture entry {name:?} must be an ordinary file"
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
            assert_eq!(metadata.nlink(), 1, "fixture entry {name:?} is hard-linked");
            assert!(
                !source_identities.contains(&(metadata.dev(), metadata.ino())),
                "fixture entry {name:?} aliases a source inode"
            );
        }
    }
    assert!(essential.into_iter().all(|present| present));
    (source_manifest, copied)
}

fn check_boundary(dir: &Path, boundary: &str) {
    assert_eq!(
        fs::metadata(dir.join("wal")).unwrap().len() == 0,
        boundary == "checkpointed",
        "WAL handoff boundary"
    );
}

fn generate(family: Family, lifecycle: Lifecycle, dir: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    assert!(!dir.exists(), "generate destination must be new");
    let mut db = Database::create(dir, cfg()).unwrap();
    populate(&mut db, family, lifecycle);
    db.commit().unwrap();
    check_database(&db, family, lifecycle, DataState::Original, false);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    check_boundary(dir, boundary);
    assert_required_features(dir, family, u32::from(lifecycle != Lifecycle::PostDrop));
    assert_catalog_presence(dir, family, lifecycle != Lifecycle::PostDrop);
    assert_eq!(
        derived_entry_count(dir, family),
        expected_derived_entries(family, lifecycle, DataState::Original, false)
    );
    let physical = physical_features(dir);
    assert_eq!(physical, [u64::from(create_compact_cells()); 2]);
    write_manifest(dir, family, lifecycle, boundary, physical);
}

fn verify(dir: &Path, state: DataState) {
    let (_, family, lifecycle, expected_physical) = validate_manifest(&dir.join(MANIFEST));
    assert_eq!(physical_features(dir), expected_physical);
    let upgraded = state != DataState::Original;
    let db = Database::open_snapshot(dir, cfg()).unwrap();
    check_database(&db, family, lifecycle, state, upgraded);
    drop(db);
    assert_required_features(
        dir,
        family,
        u32::from(expected_index_state(lifecycle, upgraded).is_some()),
    );
    assert_catalog_presence(
        dir,
        family,
        expected_index_state(lifecycle, upgraded).is_some(),
    );
    assert_eq!(
        derived_entry_count(dir, family),
        expected_derived_entries(family, lifecycle, state, upgraded)
    );
}

fn apply_normal_writes(db: &mut Database) {
    assert_eq!(
        db.put(PEOPLE, "p1", &document(1, DataState::Updated))
            .unwrap(),
        entity(2)
    );
    assert!(db.delete(PEOPLE, "p2").unwrap());
    assert_eq!(
        db.put(PEOPLE, "p10", &document(10, DataState::Updated))
            .unwrap(),
        entity(11)
    );
}

fn apply_roundtrip_writes(db: &mut Database) {
    assert_eq!(
        db.put(PEOPLE, "p3", &document(3, DataState::Roundtrip))
            .unwrap(),
        entity(4)
    );
    assert!(db.delete(PEOPLE, "p0").unwrap());
    assert_eq!(
        db.put(PEOPLE, "p11", &document(11, DataState::Roundtrip))
            .unwrap(),
        entity(12)
    );
}

fn resume_lifecycle(db: &mut Database, lifecycle: Lifecycle) {
    match lifecycle {
        Lifecycle::Building => loop {
            let ready = db.build_index_step(INDEX, 2).unwrap();
            db.commit().unwrap();
            if ready {
                break;
            }
        },
        Lifecycle::Dropping => loop {
            let done = db.drop_index_step(INDEX, 2).unwrap();
            db.commit().unwrap();
            if done {
                break;
            }
        },
        Lifecycle::Ready | Lifecycle::PostDrop => {}
    }
}

fn upgrade(source_manifest: &Path, copied: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    let (source_manifest, copied) = guarded_upgrade_paths(source_manifest, copied);
    let (source_bytes, family, lifecycle, expected_physical) = validate_manifest(&source_manifest);
    assert_eq!(fs::read(copied.join(MANIFEST)).unwrap(), source_bytes);
    assert_eq!(physical_features(&copied), expected_physical);
    verify(&copied, DataState::Original);
    let dropping_before =
        (lifecycle == Lifecycle::Dropping).then(|| derived_namespace(&copied, family));

    let mut db = Database::open(&copied, cfg()).unwrap();
    let old = Database::open_snapshot(&copied, cfg()).unwrap();
    apply_normal_writes(&mut db);
    db.commit().unwrap();
    drop(db);

    assert_required_features(&copied, family, u32::from(lifecycle != Lifecycle::PostDrop));
    match lifecycle {
        Lifecycle::Building => assert_building_after_writes(&copied, family),
        Lifecycle::Dropping => assert_eq!(
            derived_namespace(&copied, family),
            dropping_before.unwrap(),
            "DROPPING derived namespace changed during ordinary writes"
        ),
        Lifecycle::Ready | Lifecycle::PostDrop => {}
    }

    let mut db = Database::open(&copied, cfg()).unwrap();
    check_rows(&db, DataState::Updated);
    resume_lifecycle(&mut db, lifecycle);
    check_database(&db, family, lifecycle, DataState::Updated, true);
    check_database(&old, family, lifecycle, DataState::Original, false);
    drop(old);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);

    check_boundary(&copied, boundary);
    assert_required_features(&copied, family, u32::from(lifecycle.resolves_ready()));
    assert_eq!(physical_features(&copied), expected_physical);
    assert_catalog_presence(&copied, family, lifecycle.resolves_ready());
    verify(&copied, DataState::Updated);
}

fn continue_upgrade(source_manifest: &Path, copied: &Path, boundary: &str) {
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    let (source_manifest, copied) = guarded_upgrade_paths(source_manifest, copied);
    let (source_bytes, family, lifecycle, expected_physical) = validate_manifest(&source_manifest);
    assert_eq!(fs::read(copied.join(MANIFEST)).unwrap(), source_bytes);
    assert_eq!(physical_features(&copied), expected_physical);
    verify(&copied, DataState::Updated);

    let mut db = Database::open(&copied, cfg()).unwrap();
    let updated = Database::open_snapshot(&copied, cfg()).unwrap();
    apply_roundtrip_writes(&mut db);
    db.commit().unwrap();
    check_database(&updated, family, lifecycle, DataState::Updated, true);
    check_database(&db, family, lifecycle, DataState::Roundtrip, true);
    drop(updated);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);

    check_boundary(&copied, boundary);
    assert_eq!(physical_features(&copied), expected_physical);
    assert_required_features(&copied, family, u32::from(lifecycle.resolves_ready()));
    assert_catalog_presence(&copied, family, lifecycle.resolves_ready());
    verify(&copied, DataState::Roundtrip);
    assert_eq!(fs::read(source_manifest).unwrap(), source_bytes);
}

fn verify_writer(source_manifest: &Path, copied: &Path, state: DataState) {
    let (source_manifest, copied) = guarded_upgrade_paths(source_manifest, copied);
    let (source_bytes, family, lifecycle, expected_physical) = validate_manifest(&source_manifest);
    assert_eq!(fs::read(copied.join(MANIFEST)).unwrap(), source_bytes);
    assert_eq!(physical_features(&copied), expected_physical);
    verify(&copied, state);
    let wal_bytes = fs::metadata(copied.join("wal")).unwrap().len();

    let db = Database::open(&copied, cfg()).unwrap();
    check_database(&db, family, lifecycle, state, state != DataState::Original);
    drop(db);

    assert_eq!(fs::metadata(copied.join("wal")).unwrap().len(), wal_bytes);
    assert_eq!(physical_features(&copied), expected_physical);
    assert_required_features(
        &copied,
        family,
        u32::from(expected_index_state(lifecycle, state != DataState::Original).is_some()),
    );
    verify(&copied, state);
    assert_eq!(fs::read(source_manifest).unwrap(), source_bytes);
}

fn parse_state(value: &str) -> DataState {
    match value {
        "original" => DataState::Original,
        "updated" => DataState::Updated,
        "roundtrip" => DataState::Roundtrip,
        _ => panic!("state must be original|updated|roundtrip"),
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.as_slice() == ["--version"] {
        println!(
            "{}",
            json!({
                "harness":"phase2-lifecycle-fixture-v1",
                "status":"candidate-not-released-or-frozen",
                "regression_role":"future accepted-version lifecycle compatibility corpus",
                "released_baseline_relationship":"Phase 1 remains the actual released baseline",
                "engine_revision":option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded"),
                "rollback_cycle_version":1,
                "create_compact_cells":create_compact_cells(),
                "supported_logical_features":SUPPORTED_LOGICAL_FEATURES,
                "graph_enabled":false,
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
        [command, family, lifecycle, dir, boundary] if command == "generate" => {
            let family = Family::parse(family);
            let lifecycle = Lifecycle::parse(lifecycle);
            generate(family, lifecycle, Path::new(dir), boundary);
            println!(
                "{}",
                json!({"result":"PASS","command":"generate","family":family.name(),"lifecycle":lifecycle.name(),"database":dir,"boundary":boundary})
            );
        }
        [command, dir, state] if command == "verify" => {
            let state = parse_state(state);
            verify(Path::new(dir), state);
            println!(
                "{}",
                json!({"result":"PASS","command":"verify","database":dir,"state":state_string(state)})
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
            if command == "verify-writer"
                && state == "roundtrip"
                && confirm == "--confirm-copy" =>
        {
            verify_writer(Path::new(source), Path::new(copied), DataState::Roundtrip);
            println!(
                "{}",
                json!({"result":"PASS","command":"verify-writer","database":copied,"state":"roundtrip"})
            );
        }
        _ => panic!(
            "usage:\n  phase2_lifecycle_fixture generate scalar|exact-vector|spatial|text|quantized ready|building|dropping|post-drop NEW_DIR checkpointed|wal-pending\n  phase2_lifecycle_fixture verify EXISTING_DIR original|updated|roundtrip\n  phase2_lifecycle_fixture upgrade SOURCE_MANIFEST COPIED_DIR checkpointed|wal-pending --confirm-copy\n  phase2_lifecycle_fixture continue-upgrade SOURCE_MANIFEST COPIED_DIR checkpointed|wal-pending --confirm-copy\n  phase2_lifecycle_fixture verify-writer SOURCE_MANIFEST COPIED_DIR roundtrip --confirm-copy"
        ),
    }
}

fn state_string(state: DataState) -> &'static str {
    match state {
        DataState::Original => "original",
        DataState::Updated => "updated",
        DataState::Roundtrip => "roundtrip",
    }
}
