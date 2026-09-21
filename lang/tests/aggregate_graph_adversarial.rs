//! Adversarial properties of the two atomics that landed this week:
//! aggregation (`src/query/aggregate.rs`) and graph per-hop predicates plus
//! the reaching edge (`src/index/graph/mod.rs`, `src/query/drivers.rs`).
//!
//! Each named `#[test]` is one property from the fuzz2 brief. The engine is
//! not patched here: a disagreement with the brute-force fold or with the
//! contract is a failing test, which is the deliverable.

use sekejap_lang::SqlDatabase;
use sekejap_core::{
    collections::{
        Accumulator, AggValue, AggregateFn, AggregateInput, AggregateRequest, AggregateShape,
        BfsRequest, CandidateDriver, Cmp, CollectionId, CollectionOptions, Database, Direction,
        EdgePredicate, EntityId, GraphContextId, GroupCmp, GroupKey, GroupOrder, GroupPredicate,
        GroupRow, IndexId, OrderValue, OwnedScalarValue, PointFilter, Projection, QueryBudget,
        QueryDriver, QueryError, QueryFilter, QueryOrder, QueryRequest, QueryWork, ScalarFilter,
        ScalarValue, SortDirection, WorkResource,
    },
    spatial_math::{within_radius, Point},
};
use sekejap_lang::{SqlResult, SqlValue};
use sekejap_core::{
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
};

const NODES: usize = 2_000;
const SAMPLES: usize = 500;
const KINDS: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
const SEED: u64 = 0xF022_0002;
/// Born values above this are the two i64-near-MAX rows; ordinary folds
/// exclude them so a wrapping sum cannot hide behind a later group.
const BORN_ORDINARY: i64 = 10_000_000;
const OVERFLOW_BORN: i64 = i64::MAX / 2 + 1;
const RADIUS_METRES: f64 = 80_000.0;
const DIVISOR: i64 = 10;

// ── rng ───────────────────────────────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / ((1u64 << 53) as f64)
    }
}

// ── fixture ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BornCell {
    Missing,
    Null,
    Int(i64),
}

#[derive(Clone, Debug)]
struct Row {
    key: String,
    kind: String,
    born: BornCell,
    score: Option<f64>,
    label: String,
    lon: f64,
    lat: f64,
    person: bool,
}

#[derive(Clone, Debug)]
struct RefEdge {
    source: usize,
    destination: usize,
    context: u64,
    edge_type: u64,
    properties: Value,
}

struct Indexes {
    kind: IndexId,
    born: IndexId,
    loc: IndexId,
}

struct Fixture {
    db: Database,
    person: CollectionId,
    org: CollectionId,
    ids: Vec<EntityId>,
    rows: Vec<Row>,
    edges: Vec<RefEdge>,
    out: Vec<Vec<usize>>,
    incoming: Vec<Vec<usize>>,
    knows: sekejap_core::collections::EdgeTypeId,
    likes: sekejap_core::collections::EdgeTypeId,
    alt: GraphContextId,
    index: Indexes,
    _dir: tempfile::TempDir,
}

fn cfg() -> Config {
    Config {
        budget_bytes: 64 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn clusters() -> [(f64, f64); 4] {
    [
        (106.82, -6.17),
        (107.61, -6.91),
        (110.42, -6.97),
        (112.75, -7.25),
    ]
}

fn long_label(i: usize, rng: &mut Rng) -> String {
    // 200..=4000 bytes. Most rows share one of 40 labels so GROUP BY has a
    // known N; a tail of unique 4_000-byte keys is the groups-budget byte
    // bound's payload.
    let unique = i % 5 == 0;
    let len = if unique { 4_000 } else { 200 + (i % 20) * 40 };
    let mut s = String::with_capacity(len);
    if unique {
        s.push_str(&format!("U{i:05}-"));
    } else {
        s.push_str(&format!("S{:02}-", i % 40));
    }
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    while s.len() < len {
        s.push(alphabet[rng.below(alphabet.len())] as char);
    }
    s.truncate(len);
    s
}

fn document(row: &Row) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("kind".into(), json!(row.kind));
    match row.born {
        BornCell::Missing => {}
        BornCell::Null => {
            object.insert("born".into(), Value::Null);
        }
        BornCell::Int(v) => {
            object.insert("born".into(), json!(v));
        }
    }
    if let Some(score) = row.score {
        object.insert("score".into(), json!(score));
    }
    object.insert("label".into(), json!(row.label));
    object.insert(
        "loc".into(),
        json!({"type": "Point", "coordinates": [row.lon, row.lat]}),
    );
    Value::Object(object)
}

fn build() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let fields = vec![
        ("kind".into(), Kind::Text),
        ("born".into(), Kind::Int),
        ("score".into(), Kind::Real),
        ("label".into(), Kind::Text),
        ("loc".into(), Kind::Point),
    ];
    let person = db
        .create_collection("person", fields.clone(), CollectionOptions::default())
        .unwrap();
    let org = db
        .create_collection("org", fields, CollectionOptions::default())
        .unwrap();
    let mut rng = Rng(SEED);
    let mut ids = Vec::with_capacity(NODES);
    let mut rows = Vec::with_capacity(NODES);
    let centres = clusters();
    for i in 0..NODES {
        let is_person = i % 5 != 4;
        let collection = if is_person { person } else { org };
        let (clon, clat) = centres[i % centres.len()];
        let lon = clon + (rng.unit() - 0.5) * 0.4;
        let lat = clat + (rng.unit() - 0.5) * 0.3;
        let born = if i == 3 {
            BornCell::Int(OVERFLOW_BORN)
        } else if i == 5 {
            BornCell::Int(OVERFLOW_BORN + 1)
        } else if i % 10 == 0 {
            BornCell::Missing
        } else if i % 20 == 1 {
            BornCell::Null
        } else {
            let mag = 1_900 + (rng.next() % 120) as i64;
            if i % 7 == 0 {
                BornCell::Int(-mag)
            } else {
                BornCell::Int(mag)
            }
        };
        let score = if i % 20 >= 17 {
            None
        } else {
            Some((rng.unit() * 100.0).round() / 4.0)
        };
        let row = Row {
            key: format!("n{i:05}"),
            kind: KINDS[i % KINDS.len()].to_owned(),
            born,
            score,
            label: long_label(i, &mut rng),
            lon,
            lat,
            person: is_person,
        };
        let id = db.put(collection, &row.key, &document(&row)).unwrap();
        ids.push(id);
        rows.push(row);
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    let kind_index = db
        .create_scalar_index(person, "person_kind", "kind", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(kind_index, 256).unwrap();
    db.commit().unwrap();
    let born_index = db
        .create_scalar_index(person, "person_born", "born", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(born_index, 256).unwrap();
    db.commit().unwrap();
    let loc_index = db.create_point_index(person, "person_loc", "loc").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(loc_index, 256).unwrap();
    db.commit().unwrap();

    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let likes = db.create_edge_type("likes").unwrap();
    let alt = db.create_graph_context("alt").unwrap();
    db.commit().unwrap();

    let mut edges: Vec<RefEdge> = Vec::new();
    let mut written: BTreeSet<(usize, u64, u64, usize)> = BTreeSet::new();
    for source in 0..NODES {
        let degree = 3 + rng.below(4);
        for k in 0..degree {
            let destination = rng.below(NODES);
            if destination == source {
                continue;
            }
            let edge_type = if k % 2 == 0 { 1u64 } else { 2u64 };
            let context = u64::from(rng.below(5) == 0);
            if !written.insert((source, context, edge_type, destination)) {
                continue;
            }
            let weight_roll = rng.below(10);
            let mut props = serde_json::Map::new();
            match weight_roll {
                0 => {}
                1 => {
                    props.insert("weight".into(), Value::Null);
                }
                2 => {
                    props.insert("weight".into(), json!("heavy"));
                }
                _ => {
                    props.insert(
                        "weight".into(),
                        json!((rng.next() % 1_000) as f64 / 1_000.0),
                    );
                }
            }
            props.insert("since".into(), json!(1_990 + (rng.next() % 40) as i64));
            props.insert(
                "tag".into(),
                json!(if rng.next() % 2 == 0 { "even" } else { "odd" }),
            );
            let properties = Value::Object(props);
            let context_id = if context == 0 {
                GraphContextId::BASE
            } else {
                alt
            };
            let ty = if edge_type == 1 { knows } else { likes };
            db.put_edge(context_id, ids[source], ty, ids[destination], &properties)
                .unwrap();
            edges.push(RefEdge {
                source,
                destination,
                context,
                edge_type,
                properties,
            });
        }
        if source % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    let mut out = vec![Vec::new(); NODES];
    let mut incoming = vec![Vec::new(); NODES];
    for (at, edge) in edges.iter().enumerate() {
        out[edge.source].push(at);
        incoming[edge.destination].push(at);
    }
    let by_key = |ids: &Vec<EntityId>, list: &mut Vec<usize>, edges: &Vec<RefEdge>, far: bool| {
        list.sort_by_key(|at| {
            let edge = &edges[*at];
            let node = if far { edge.destination } else { edge.source };
            (edge.context, edge.edge_type, ids[node])
        });
    };
    for list in &mut out {
        by_key(&ids, list, &edges, true);
    }
    for list in &mut incoming {
        by_key(&ids, list, &edges, false);
    }

    Fixture {
        db,
        person,
        org,
        ids,
        rows,
        edges,
        out,
        incoming,
        knows,
        likes,
        alt,
        index: Indexes {
            kind: kind_index,
            born: born_index,
            loc: loc_index,
        },
        _dir: dir,
    }
}

thread_local! {
    static FIXTURE: RefCell<Fixture> = RefCell::new(build());
}

fn with_fixture<T>(f: impl FnOnce(&mut Fixture) -> T) -> T {
    FIXTURE.with(|cell| f(&mut cell.borrow_mut()))
}

// ── scalar / fold helpers ─────────────────────────────────────────────────

fn type_rank(value: &OwnedScalarValue) -> u8 {
    match value {
        OwnedScalarValue::Nullish => 0,
        OwnedScalarValue::Bool(_) => 1,
        OwnedScalarValue::I64(_) | OwnedScalarValue::F64(_) => 2,
        OwnedScalarValue::Text(_) => 3,
    }
}

fn compare_scalar(left: &OwnedScalarValue, right: &OwnedScalarValue) -> Ordering {
    match (left, right) {
        (OwnedScalarValue::I64(a), OwnedScalarValue::I64(b)) => a.cmp(b),
        (OwnedScalarValue::F64(a), OwnedScalarValue::F64(b)) => a.total_cmp(b),
        (OwnedScalarValue::I64(a), OwnedScalarValue::F64(b)) => (*a as f64).total_cmp(b),
        (OwnedScalarValue::F64(a), OwnedScalarValue::I64(b)) => a.total_cmp(&(*b as f64)),
        (OwnedScalarValue::Text(a), OwnedScalarValue::Text(b)) => a.cmp(b),
        (OwnedScalarValue::Bool(a), OwnedScalarValue::Bool(b)) => a.cmp(b),
        (a, b) => type_rank(a).cmp(&type_rank(b)),
    }
}

fn born_key(cell: BornCell) -> OwnedScalarValue {
    match cell {
        BornCell::Missing | BornCell::Null => OwnedScalarValue::Nullish,
        BornCell::Int(v) => OwnedScalarValue::I64(v),
    }
}

fn centre_of(f: &Fixture) -> Point {
    let row = f.rows.iter().find(|r| r.person).unwrap();
    Point::new(row.lon, row.lat).unwrap()
}

#[derive(Clone, Copy, Debug)]
enum FilterShape {
    None,
    BornRange,
    KindEq,
    Radius,
    OrdinaryBorn,
}

impl FilterShape {
    fn filters<'a>(self, f: &'a Fixture, kind: &'a str) -> Vec<QueryFilter<'a>> {
        match self {
            FilterShape::None => Vec::new(),
            FilterShape::BornRange => vec![QueryFilter::Scalar {
                index: f.index.born,
                predicate: ScalarFilter::Range {
                    lower: Bound::Included(ScalarValue::I64(1_900)),
                    upper: Bound::Included(ScalarValue::I64(2_020)),
                },
            }],
            FilterShape::KindEq => vec![QueryFilter::Scalar {
                index: f.index.kind,
                predicate: ScalarFilter::Eq(ScalarValue::Text(kind)),
            }],
            FilterShape::Radius => vec![QueryFilter::Point {
                index: f.index.loc,
                predicate: PointFilter::Radius {
                    center: centre_of(f),
                    radius_metres: RADIUS_METRES,
                },
            }],
            FilterShape::OrdinaryBorn => vec![QueryFilter::Scalar {
                index: f.index.born,
                predicate: ScalarFilter::Range {
                    lower: Bound::Included(ScalarValue::I64(i64::MIN)),
                    upper: Bound::Excluded(ScalarValue::I64(BORN_ORDINARY)),
                },
            }],
        }
    }

    fn admits(self, f: &Fixture, row: &Row) -> bool {
        if !row.person {
            return false;
        }
        match self {
            FilterShape::None => true,
            FilterShape::BornRange => match row.born {
                BornCell::Int(v) => (1_900..=2_020).contains(&v),
                _ => false,
            },
            FilterShape::KindEq => row.kind == "alpha",
            FilterShape::Radius => {
                let point = Point::new(row.lon, row.lat).unwrap();
                within_radius(centre_of(f), point, RADIUS_METRES).unwrap()
            }
            FilterShape::OrdinaryBorn => match row.born {
                BornCell::Int(v) => v < BORN_ORDINARY,
                _ => false,
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Grouping {
    None,
    IndexedKind,
    FieldKind,
    FieldLabel,
    IndexedBorn,
    DividedBorn,
}

impl Grouping {
    fn key(self, index: &Indexes) -> Option<GroupKey<'static>> {
        match self {
            Grouping::None => None,
            Grouping::IndexedKind => Some(GroupKey::Index(index.kind)),
            Grouping::FieldKind => Some(GroupKey::Field("kind")),
            Grouping::FieldLabel => Some(GroupKey::Field("label")),
            Grouping::IndexedBorn => Some(GroupKey::Index(index.born)),
            Grouping::DividedBorn => Some(GroupKey::IndexDiv {
                index: index.born,
                divisor: DIVISOR,
            }),
        }
    }

    fn of(self, row: &Row) -> Option<OwnedScalarValue> {
        match self {
            Grouping::None => None,
            Grouping::IndexedKind | Grouping::FieldKind => {
                Some(OwnedScalarValue::Text(row.kind.clone()))
            }
            Grouping::FieldLabel => Some(OwnedScalarValue::Text(row.label.clone())),
            Grouping::IndexedBorn => Some(born_key(row.born)),
            Grouping::DividedBorn => match row.born {
                BornCell::Int(v) => Some(OwnedScalarValue::I64(v / DIVISOR)),
                BornCell::Missing | BornCell::Null => Some(OwnedScalarValue::Nullish),
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum AccWant {
    CountStar,
    CountBorn,
    CountScore,
    CountLabel,
    SumBorn,
    MinBorn,
    MaxBorn,
    AvgBorn,
    MinLabel,
    MaxLabel,
    MinScore,
    MaxScore,
    SumScore,
    AvgScore,
}

impl AccWant {
    fn accumulator(self, index: &Indexes) -> Accumulator<'static> {
        match self {
            AccWant::CountStar => Accumulator {
                function: AggregateFn::CountStar,
                input: None,
            },
            AccWant::CountBorn => Accumulator {
                function: AggregateFn::Count,
                input: Some(AggregateInput::Index(index.born)),
            },
            AccWant::CountScore => Accumulator {
                function: AggregateFn::Count,
                input: Some(AggregateInput::Field("score")),
            },
            AccWant::CountLabel => Accumulator {
                function: AggregateFn::Count,
                input: Some(AggregateInput::Field("label")),
            },
            AccWant::SumBorn => Accumulator {
                function: AggregateFn::Sum,
                input: Some(AggregateInput::Index(index.born)),
            },
            AccWant::MinBorn => Accumulator {
                function: AggregateFn::Min,
                input: Some(AggregateInput::Index(index.born)),
            },
            AccWant::MaxBorn => Accumulator {
                function: AggregateFn::Max,
                input: Some(AggregateInput::Index(index.born)),
            },
            AccWant::AvgBorn => Accumulator {
                function: AggregateFn::Avg,
                input: Some(AggregateInput::Index(index.born)),
            },
            AccWant::MinLabel => Accumulator {
                function: AggregateFn::Min,
                input: Some(AggregateInput::Field("label")),
            },
            AccWant::MaxLabel => Accumulator {
                function: AggregateFn::Max,
                input: Some(AggregateInput::Field("label")),
            },
            AccWant::MinScore => Accumulator {
                function: AggregateFn::Min,
                input: Some(AggregateInput::Field("score")),
            },
            AccWant::MaxScore => Accumulator {
                function: AggregateFn::Max,
                input: Some(AggregateInput::Field("score")),
            },
            AccWant::SumScore => Accumulator {
                function: AggregateFn::Sum,
                input: Some(AggregateInput::Field("score")),
            },
            AccWant::AvgScore => Accumulator {
                function: AggregateFn::Avg,
                input: Some(AggregateInput::Field("score")),
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Folded {
    count_star: u64,
    count_born: u64,
    count_score: u64,
    count_label: u64,
    sum_born: i128,
    min_born: Option<i64>,
    max_born: Option<i64>,
    sum_score: f64,
    n_score: u64,
    min_score: Option<f64>,
    max_score: Option<f64>,
    min_label: Option<String>,
    max_label: Option<String>,
}

fn fold_groups(f: &Fixture, filter: FilterShape, grouping: Grouping) -> BTreeMap<GroupOrd, Folded> {
    let mut out: BTreeMap<GroupOrd, Folded> = BTreeMap::new();
    for row in &f.rows {
        if !filter.admits(f, row) {
            continue;
        }
        let key = grouping.of(row);
        let slot = out.entry(GroupOrd(key)).or_default();
        slot.count_star += 1;
        if matches!(row.born, BornCell::Int(_)) {
            slot.count_born += 1;
        }
        if row.score.is_some() {
            slot.count_score += 1;
        }
        slot.count_label += 1;
        if let BornCell::Int(v) = row.born {
            slot.sum_born += i128::from(v);
            slot.min_born = Some(slot.min_born.map_or(v, |old| old.min(v)));
            slot.max_born = Some(slot.max_born.map_or(v, |old| old.max(v)));
        }
        if let Some(score) = row.score {
            slot.sum_score += score;
            slot.n_score += 1;
            slot.min_score = Some(slot.min_score.map_or(score, |old| {
                if score.total_cmp(&old) == Ordering::Less {
                    score
                } else {
                    old
                }
            }));
            slot.max_score = Some(slot.max_score.map_or(score, |old| {
                if score.total_cmp(&old) == Ordering::Greater {
                    score
                } else {
                    old
                }
            }));
        }
        match &slot.min_label {
            None => slot.min_label = Some(row.label.clone()),
            Some(cur) if row.label.as_str() < cur.as_str() => {
                slot.min_label = Some(row.label.clone())
            }
            _ => {}
        }
        match &slot.max_label {
            None => slot.max_label = Some(row.label.clone()),
            Some(cur) if row.label.as_str() > cur.as_str() => {
                slot.max_label = Some(row.label.clone())
            }
            _ => {}
        }
    }
    out
}

#[derive(Clone, Debug)]
struct GroupOrd(Option<OwnedScalarValue>);

impl PartialEq for GroupOrd {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => compare_scalar(a, b) == Ordering::Equal,
            _ => false,
        }
    }
}
impl Eq for GroupOrd {}
impl PartialOrd for GroupOrd {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for GroupOrd {
    fn cmp(&self, other: &Self) -> Ordering {
        match (&self.0, &other.0) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(a), Some(b)) => compare_scalar(a, b),
        }
    }
}

fn folded_value(acc: AccWant, g: &Folded) -> Result<AggValue, &'static str> {
    Ok(match acc {
        AccWant::CountStar => AggValue::Count(g.count_star),
        AccWant::CountBorn => AggValue::Count(g.count_born),
        AccWant::CountScore => AggValue::Count(g.count_score),
        AccWant::CountLabel => AggValue::Count(g.count_label),
        AccWant::SumBorn => {
            if g.count_born == 0 {
                AggValue::Null
            } else {
                AggValue::I64(
                    i64::try_from(g.sum_born)
                        .map_err(|_| "sum exceeds the range of a 64-bit integer")?,
                )
            }
        }
        AccWant::MinBorn => match g.min_born {
            None => AggValue::Null,
            Some(v) => AggValue::I64(v),
        },
        AccWant::MaxBorn => match g.max_born {
            None => AggValue::Null,
            Some(v) => AggValue::I64(v),
        },
        AccWant::AvgBorn => {
            if g.count_born == 0 {
                AggValue::Null
            } else {
                AggValue::F64(g.sum_born as f64 / g.count_born as f64)
            }
        }
        AccWant::MinLabel => match &g.min_label {
            None => AggValue::Null,
            Some(v) => AggValue::Text(v.clone()),
        },
        AccWant::MaxLabel => match &g.max_label {
            None => AggValue::Null,
            Some(v) => AggValue::Text(v.clone()),
        },
        AccWant::MinScore => match g.min_score {
            None => AggValue::Null,
            Some(v) => AggValue::F64(v),
        },
        AccWant::MaxScore => match g.max_score {
            None => AggValue::Null,
            Some(v) => AggValue::F64(v),
        },
        AccWant::SumScore => {
            if g.n_score == 0 {
                AggValue::Null
            } else {
                AggValue::F64(g.sum_score)
            }
        }
        AccWant::AvgScore => {
            if g.n_score == 0 {
                AggValue::Null
            } else {
                AggValue::F64(g.sum_score / g.n_score as f64)
            }
        }
    })
}

fn agg_eq(got: &AggValue, want: &AggValue, ctx: &str) {
    match (got, want) {
        (AggValue::F64(a), AggValue::F64(b)) => {
            let tol = 1e-9_f64.max(b.abs() * 1e-9);
            assert!(
                (a - b).abs() <= tol || a.total_cmp(b) == Ordering::Equal,
                "{ctx}: avg/real {a} against {b}"
            );
        }
        _ => assert_eq!(got, want, "{ctx}"),
    }
}

fn drain_agg(
    prepared: &mut sekejap_core::collections::PreparedAggregate<'_>,
    page_size: usize,
    budget: QueryBudget,
) -> Result<Vec<GroupRow>, QueryError> {
    drain_agg_cancel(prepared, page_size, budget, || false)
}

fn drain_agg_cancel<C: FnMut() -> bool>(
    prepared: &mut sekejap_core::collections::PreparedAggregate<'_>,
    page_size: usize,
    budget: QueryBudget,
    mut cancelled: C,
) -> Result<Vec<GroupRow>, QueryError> {
    let mut out = Vec::new();
    loop {
        let page = prepared.next_page(page_size, budget, &mut cancelled)?;
        let empty = page.groups.is_empty();
        out.extend(page.groups);
        if page.done || empty {
            break;
        }
    }
    Ok(out)
}

fn run_agg(
    f: &Fixture,
    filter: FilterShape,
    grouping: Grouping,
    accs: &[AccWant],
    having: &[GroupPredicate],
    order: GroupOrder,
    total_limit: Option<usize>,
    page_size: usize,
    driver: CandidateDriver,
) -> Result<(Vec<GroupRow>, AggregateShape), QueryError> {
    let filters = filter.filters(f, "alpha");
    let accumulators: Vec<Accumulator<'static>> =
        accs.iter().map(|a| a.accumulator(&f.index)).collect();
    let mut prepared = f.db.prepare_aggregate(AggregateRequest {
        collection: f.person,
        filters: &filters,
        group: grouping.key(&f.index),
        accumulators: &accumulators,
        having,
        order,
        driver,
        total_limit,
    })?;
    let shape = prepared.shape();
    let groups = drain_agg(&mut prepared, page_size, QueryBudget::unlimited())?;
    Ok((groups, shape))
}

fn check_fold(
    f: &Fixture,
    filter: FilterShape,
    grouping: Grouping,
    accs: &[AccWant],
    groups: &[GroupRow],
) {
    let folded = fold_groups(f, filter, grouping);
    let mut expected: Vec<(Option<OwnedScalarValue>, Vec<AggValue>)> = Vec::new();
    for (key, g) in &folded {
        let mut values = Vec::new();
        for acc in accs {
            match folded_value(*acc, g) {
                Ok(v) => values.push(v),
                Err(msg) => panic!(
                    "{filter:?}/{grouping:?}: fold overflowed ({msg}) but the engine returned {} groups",
                    groups.len()
                ),
            }
        }
        expected.push((key.0.clone(), values));
    }
    assert_eq!(
        groups.len(),
        expected.len(),
        "{filter:?}/{grouping:?}: group count engine {} fold {}",
        groups.len(),
        expected.len()
    );
    for (got, (key, values)) in groups.iter().zip(&expected) {
        assert_eq!(got.key, *key, "{filter:?}/{grouping:?}: key");
        assert_eq!(got.values.len(), values.len());
        for (i, (gv, wv)) in got.values.iter().zip(values).enumerate() {
            agg_eq(gv, wv, &format!("{filter:?}/{grouping:?}/{key:?}/acc{i}"));
        }
    }
}

/// battle50k `agg_line`: the group key, then `|name=value` per accumulator.
/// `avg` is compared as `floor(avg)`.
fn agg_line(key: &str, fields: &[(&str, i64)]) -> String {
    let mut out = key.to_owned();
    for (name, value) in fields {
        out.push('|');
        out.push_str(name);
        out.push('=');
        out.push_str(&value.to_string());
    }
    out
}

fn agg_number(value: &AggValue) -> i64 {
    match value {
        AggValue::Count(n) => *n as i64,
        AggValue::I64(v) => *v,
        AggValue::F64(v) => v.floor() as i64,
        AggValue::Bool(v) => i64::from(*v),
        AggValue::Null => panic!("agg_line cannot print NULL"),
        AggValue::Text(_) => panic!("agg_line cannot print text"),
    }
}

fn sql_number(value: &SqlValue) -> i64 {
    match value {
        SqlValue::Int(v) => *v,
        SqlValue::Float(v) => v.floor() as i64,
        SqlValue::Bool(v) => i64::from(*v),
        other => panic!("sql agg_line got {other:?}"),
    }
}

fn sql_key(value: &SqlValue) -> String {
    match value {
        SqlValue::Null | SqlValue::Missing => "NULL".to_owned(),
        SqlValue::Text(t) => t.clone(),
        SqlValue::Int(v) => v.to_string(),
        SqlValue::Float(v) => format!("{v}"),
        SqlValue::Bool(v) => v.to_string(),
        other => panic!("sql group key {other:?}"),
    }
}

fn api_lines(groups: &[GroupRow], names: &[&str]) -> Vec<String> {
    groups
        .iter()
        .map(|row| {
            let key = match &row.key {
                None => String::new(),
                Some(OwnedScalarValue::Text(t)) => t.clone(),
                Some(OwnedScalarValue::I64(v)) => v.to_string(),
                Some(OwnedScalarValue::F64(v)) => format!("{v}"),
                Some(OwnedScalarValue::Bool(v)) => v.to_string(),
                Some(OwnedScalarValue::Nullish) => "NULL".to_owned(),
            };
            let fields: Vec<(&str, i64)> = names
                .iter()
                .enumerate()
                .map(|(i, name)| (*name, agg_number(&row.values[i])))
                .collect();
            agg_line(&key, &fields)
        })
        .collect()
}

fn sql_lines(db: &mut Database, sql: &str, names: &[&str]) -> Vec<String> {
    match db.sql(sql, &[]).unwrap() {
        SqlResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| {
                let (key, rest) = if names.len() + 1 == row.values.len() {
                    (sql_key(&row.values[0]), &row.values[1..])
                } else {
                    (String::new(), row.values.as_slice())
                };
                let fields: Vec<(&str, i64)> = names
                    .iter()
                    .zip(rest.iter())
                    .map(|(n, v)| (*n, sql_number(v)))
                    .collect();
                agg_line(&key, &fields)
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

// ── graph reference ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct GQuestion {
    seed: usize,
    direction: Direction,
    context: u64,
    edge_type: Option<u64>,
    min_depth: usize,
    max_depth: usize,
    edge_where: Vec<(String, Cmp, Value)>,
    born: Option<(i64, i64)>,
    born_eq: Option<i64>,
    radius: Option<(f64, f64, f64)>,
}

fn property_matches(properties: &Value, property: &str, op: Cmp, value: &Value) -> bool {
    let Some(found) = properties.get(property) else {
        return false;
    };
    let ordering = match (found, value) {
        (Value::Number(a), Value::Number(b)) => {
            match a.as_f64().unwrap().partial_cmp(&b.as_f64().unwrap()) {
                Some(ordering) => ordering,
                None => return false,
            }
        }
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => return false,
    };
    match op {
        Cmp::Eq => ordering.is_eq(),
        Cmp::Ne => !ordering.is_eq(),
        Cmp::Lt => ordering.is_lt(),
        Cmp::Le => ordering.is_le(),
        Cmp::Gt => ordering.is_gt(),
        Cmp::Ge => ordering.is_ge(),
    }
}

impl Fixture {
    fn node_admits(&self, q: &GQuestion, node: usize) -> bool {
        if q.born.is_none() && q.born_eq.is_none() && q.radius.is_none() {
            return true;
        }
        if self.ids[node].collection != self.person {
            return false;
        }
        if let Some((lower, upper)) = q.born {
            match self.rows[node].born {
                BornCell::Int(v) if v >= lower && v <= upper => {}
                _ => return false,
            }
        }
        if let Some(year) = q.born_eq {
            match self.rows[node].born {
                BornCell::Int(v) if v == year => {}
                _ => return false,
            }
        }
        if let Some((lon, lat, metres)) = q.radius {
            let row = &self.rows[node];
            if !within_radius(
                Point::new(lon, lat).unwrap(),
                Point::new(row.lon, row.lat).unwrap(),
                metres,
            )
            .unwrap()
            {
                return false;
            }
        }
        true
    }

    fn edge_admits(&self, q: &GQuestion, edge: &RefEdge) -> bool {
        if let Some(ty) = q.edge_type {
            if edge.edge_type != ty {
                return false;
            }
        }
        q.edge_where
            .iter()
            .all(|(property, op, value)| property_matches(&edge.properties, property, *op, value))
    }

    fn reference(&self, q: &GQuestion) -> Vec<(usize, usize, usize)> {
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        seen.insert(q.seed);
        let mut frontier = vec![q.seed];
        let mut out: Vec<(usize, usize, usize)> = Vec::new();
        for depth in 1..=q.max_depth {
            let mut offers: Vec<(usize, usize)> = Vec::new();
            for node in &frontier {
                let mut lists: Vec<(&Vec<usize>, bool)> = Vec::new();
                if matches!(q.direction, Direction::Outgoing | Direction::Both) {
                    lists.push((&self.out[*node], true));
                }
                if matches!(q.direction, Direction::Incoming | Direction::Both) {
                    lists.push((&self.incoming[*node], false));
                }
                for (list, forward) in lists {
                    for at in list {
                        let edge = &self.edges[*at];
                        if edge.context != q.context {
                            continue;
                        }
                        let far = if forward {
                            edge.destination
                        } else {
                            edge.source
                        };
                        if seen.contains(&far) {
                            continue;
                        }
                        if !self.edge_admits(q, edge) {
                            continue;
                        }
                        if !self.node_admits(q, far) {
                            continue;
                        }
                        offers.push((far, *at));
                    }
                }
            }
            if offers.is_empty() {
                break;
            }
            let mut level: BTreeMap<usize, usize> = BTreeMap::new();
            for (node, at) in offers {
                level.entry(node).or_insert(at);
            }
            let mut next: Vec<(usize, usize)> = level.into_iter().collect();
            next.sort_by_key(|(node, _)| self.ids[*node]);
            for (node, at) in &next {
                seen.insert(*node);
                if depth >= q.min_depth {
                    out.push((*node, depth, *at));
                }
            }
            frontier = next.into_iter().map(|(node, _)| node).collect();
        }
        out.sort_by_key(|(node, _, _)| self.ids[*node]);
        out
    }

    fn context_id(&self, context: u64) -> GraphContextId {
        if context == 0 {
            GraphContextId::BASE
        } else {
            self.alt
        }
    }

    fn edge_type_id(&self, ty: Option<u64>) -> Option<sekejap_core::collections::EdgeTypeId> {
        match ty {
            None => None,
            Some(1) => Some(self.knows),
            Some(2) => Some(self.likes),
            _ => None,
        }
    }

    fn engine(
        &self,
        q: &GQuestion,
    ) -> Result<Vec<(EntityId, usize, Option<Value>)>, sekejap_core::collections::Error> {
        let edge_where: Vec<EdgePredicate<'_>> = q
            .edge_where
            .iter()
            .map(|(property, op, value)| EdgePredicate {
                property,
                op: *op,
                value: scalar_of(value),
            })
            .collect();
        let node_where = self.node_filters(q);
        let request = BfsRequest {
            seed: self.ids[q.seed],
            direction: q.direction,
            context: self.context_id(q.context),
            edge_type: self.edge_type_id(q.edge_type),
            min_depth: q.min_depth,
            max_depth: q.max_depth,
            include_seed: false,
            max_visited: 65_536,
            max_edges: 1_000_000,
            result_limit: 65_536,
            edge_where: &edge_where,
            node_where: &node_where,
        };
        let mut out: Vec<(EntityId, usize, Option<Value>)> = self
            .db
            .traverse_bfs_binding_edges(request)?
            .nodes
            .into_iter()
            .map(|node| (node.entity, node.depth, node.via.map(|via| via.properties)))
            .collect();
        out.sort_unstable_by_key(|(id, _, _)| *id);
        Ok(out)
    }

    fn node_filters(&self, q: &GQuestion) -> Vec<QueryFilter<'static>> {
        let mut filters: Vec<QueryFilter<'static>> = Vec::new();
        if let Some((lower, upper)) = q.born {
            filters.push(QueryFilter::Scalar {
                index: self.index.born,
                predicate: ScalarFilter::Range {
                    lower: Bound::Included(ScalarValue::I64(lower)),
                    upper: Bound::Included(ScalarValue::I64(upper)),
                },
            });
        }
        if let Some(year) = q.born_eq {
            filters.push(QueryFilter::Scalar {
                index: self.index.born,
                predicate: ScalarFilter::Eq(ScalarValue::I64(year)),
            });
        }
        if let Some((lon, lat, radius_metres)) = q.radius {
            filters.push(QueryFilter::Point {
                index: self.index.loc,
                predicate: PointFilter::Radius {
                    center: Point::new(lon, lat).unwrap(),
                    radius_metres,
                },
            });
        }
        filters
    }
}

fn scalar_of(value: &Value) -> ScalarValue<'_> {
    match value {
        Value::Bool(value) => ScalarValue::Bool(*value),
        Value::String(value) => ScalarValue::Text(value),
        Value::Number(number) => match number.as_i64() {
            Some(value) => ScalarValue::I64(value),
            None => ScalarValue::F64(number.as_f64().unwrap()),
        },
        other => panic!("not a scalar: {other}"),
    }
}

fn questions(f: &Fixture) -> Vec<GQuestion> {
    let mut rng = Rng(0xC0FF_EE02);
    let mut out = Vec::with_capacity(SAMPLES);
    while out.len() < SAMPLES {
        let seed = rng.below(NODES);
        let direction = match rng.below(3) {
            0 => Direction::Outgoing,
            1 => Direction::Incoming,
            _ => Direction::Both,
        };
        let context = u64::from(rng.below(5) == 0);
        let max_depth = 1 + rng.below(3);
        let min_depth = if max_depth >= 2 && rng.below(3) == 0 {
            2 + rng.below(max_depth - 1)
        } else {
            1
        };
        let edge_type = match rng.below(3) {
            0 => Some(1u64),
            1 => Some(2u64),
            _ => None,
        };
        let mut edge_where = Vec::new();
        match rng.below(8) {
            0 => {}
            1 => edge_where.push(("weight".to_owned(), Cmp::Gt, json!(rng.unit() * 0.5))),
            2 => edge_where.push(("weight".to_owned(), Cmp::Lt, json!(0.7))),
            3 => edge_where.push(("weight".to_owned(), Cmp::Eq, json!("heavy"))),
            4 => edge_where.push(("weight".to_owned(), Cmp::Ne, json!(0.5))),
            5 => edge_where.push((
                "since".to_owned(),
                Cmp::Lt,
                json!(2_000 + rng.below(20) as i64),
            )),
            6 => edge_where.push((
                "tag".to_owned(),
                if rng.below(2) == 0 { Cmp::Eq } else { Cmp::Ne },
                json!("even"),
            )),
            _ => {
                edge_where.push(("weight".to_owned(), Cmp::Eq, json!(0.25)));
                edge_where.push(("since".to_owned(), Cmp::Gt, json!(1_995i64)));
            }
        }
        let (mut born, mut born_eq) = (None, None);
        if rng.below(3) == 0 {
            if rng.below(3) == 0 {
                born_eq = Some(1_900 + rng.below(120) as i64);
            } else {
                let lower = 1_900 + rng.below(80) as i64;
                born = Some((lower, lower + rng.below(40) as i64));
            }
        }
        let mut radius = None;
        if rng.below(4) == 0 {
            let (lon, lat) = (f.rows[rng.below(NODES)].lon, f.rows[rng.below(NODES)].lat);
            radius = Some((lon, lat, 20_000.0 + rng.unit() * 200_000.0));
        }
        out.push(GQuestion {
            seed,
            direction,
            context,
            edge_type,
            min_depth,
            max_depth,
            edge_where,
            born,
            born_eq,
            radius,
        });
    }
    out
}

fn work_of(
    f: &Fixture,
    seed: usize,
    direction: Direction,
    max_depth: usize,
    max_edges: usize,
    edge_where: &[EdgePredicate<'_>],
    node_where: &[QueryFilter<'_>],
    edge_type: Option<sekejap_core::collections::EdgeTypeId>,
) -> Result<(QueryWork, u64), QueryError> {
    let filters = [QueryFilter::Graph(BfsRequest {
        seed: f.ids[seed],
        direction,
        context: GraphContextId::BASE,
        edge_type,
        min_depth: 1,
        max_depth,
        include_seed: false,
        max_visited: 65_536,
        max_edges,
        result_limit: 65_536,
        edge_where,
        node_where,
    })];
    let mut prepared = f.db.prepare_query(QueryRequest {
        collection: f.person,
        filters: &filters,
        order: QueryOrder::Driver,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Auto,
    })?;
    let mut total = QueryWork::default();
    let mut rows = 0u64;
    loop {
        let page = prepared.next_page(4_096, QueryBudget::unlimited(), || false)?;
        total.candidates += page.work.candidates;
        total.primary_reads += page.work.primary_reads;
        total.row_decodes += page.work.row_decodes;
        total.graph_edges += page.work.graph_edges;
        total.graph_visited += page.work.graph_visited;
        rows += page.rows.len() as u64;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    Ok((total, rows))
}

fn collect_ids(
    f: &Fixture,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    page_size: usize,
    driver: CandidateDriver,
    limit: Option<usize>,
) -> Result<Vec<(EntityId, OrderValue)>, QueryError> {
    let mut prepared = f.db.prepare_query(QueryRequest {
        collection: f.person,
        filters,
        order,
        projection: Projection::Ids,
        total_limit: limit,
        driver,
    })?;
    let mut out = Vec::new();
    loop {
        let page = prepared.next_page(page_size, QueryBudget::unlimited(), || false)?;
        let empty = page.rows.is_empty();
        let done = page.done;
        out.extend(page.rows.into_iter().map(|row| (row.id, row.order)));
        if done || empty {
            break;
        }
    }
    Ok(out)
}

// ── (a) ───────────────────────────────────────────────────────────────────

#[test]
fn a_every_accumulator_over_every_filter_shape_equals_the_fold() {
    with_fixture(|f| {
        let accs = [
            AccWant::CountStar,
            AccWant::CountBorn,
            AccWant::CountScore,
            AccWant::CountLabel,
            AccWant::MinBorn,
            AccWant::MaxBorn,
            AccWant::AvgBorn,
            AccWant::MinLabel,
            AccWant::MaxLabel,
            AccWant::MinScore,
            AccWant::MaxScore,
            AccWant::SumScore,
            AccWant::AvgScore,
        ];
        let with_sum = [
            AccWant::CountStar,
            AccWant::SumBorn,
            AccWant::MinBorn,
            AccWant::MaxBorn,
            AccWant::AvgBorn,
        ];
        let filters = [
            FilterShape::OrdinaryBorn,
            FilterShape::BornRange,
            FilterShape::KindEq,
            FilterShape::Radius,
        ];
        let groupings = [
            Grouping::None,
            Grouping::IndexedKind,
            Grouping::FieldKind,
            Grouping::IndexedBorn,
        ];
        for filter in filters {
            for grouping in groupings {
                let (groups, _) = run_agg(
                    f,
                    filter,
                    grouping,
                    &accs,
                    &[],
                    GroupOrder::Key,
                    None,
                    8_192,
                    CandidateDriver::Auto,
                )
                .unwrap_or_else(|e| panic!("{filter:?}/{grouping:?}: {e}"));
                assert!(
                    !groups.is_empty(),
                    "{filter:?}/{grouping:?} admitted no groups"
                );
                check_fold(f, filter, grouping, &accs, &groups);
            }
        }
        for grouping in groupings {
            let (groups, _) = run_agg(
                f,
                FilterShape::OrdinaryBorn,
                grouping,
                &with_sum,
                &[],
                GroupOrder::Key,
                None,
                8_192,
                CandidateDriver::Auto,
            )
            .unwrap_or_else(|e| panic!("sum/{grouping:?}: {e}"));
            check_fold(f, FilterShape::OrdinaryBorn, grouping, &with_sum, &groups);
        }

        // Missing vs JSON null are one group under GROUP BY born.
        let (groups, _) = run_agg(
            f,
            FilterShape::None,
            Grouping::IndexedBorn,
            &[AccWant::CountStar, AccWant::CountBorn],
            &[],
            GroupOrder::Key,
            None,
            8_192,
            CandidateDriver::Auto,
        )
        .unwrap();
        let nullish = groups
            .iter()
            .find(|g| g.key == Some(OwnedScalarValue::Nullish))
            .expect("missing and JSON null must share one group");
        let expected_nullish = f
            .rows
            .iter()
            .filter(|r| r.person && matches!(r.born, BornCell::Missing | BornCell::Null))
            .count() as u64;
        match &nullish.values[0] {
            AggValue::Count(n) => assert_eq!(*n, expected_nullish, "nullish group count(*)"),
            other => panic!("{other:?}"),
        }
        match &nullish.values[1] {
            AggValue::Count(n) => assert_eq!(*n, 0, "count(born) over the nullish group"),
            other => panic!("{other:?}"),
        }

        // All-null groups: sum/min/max/avg of born on the nullish group is NULL.
        let (groups, _) = run_agg(
            f,
            FilterShape::None,
            Grouping::IndexedBorn,
            &[AccWant::SumBorn, AccWant::MinBorn, AccWant::AvgBorn],
            &[],
            GroupOrder::Key,
            None,
            8_192,
            CandidateDriver::Auto,
        )
        .unwrap();
        let nullish = groups
            .iter()
            .find(|g| g.key == Some(OwnedScalarValue::Nullish))
            .unwrap();
        assert_eq!(nullish.values[0], AggValue::Null);
        assert_eq!(nullish.values[1], AggValue::Null);
        assert_eq!(nullish.values[2], AggValue::Null);

        // i64 sums near i64::MAX must be an error, not a wrap.
        let acc = [AccWant::SumBorn];
        let err = run_agg(
            f,
            FilterShape::None,
            Grouping::None,
            &acc,
            &[],
            GroupOrder::Key,
            None,
            8_192,
            CandidateDriver::Auto,
        )
        .expect_err("sum of two (i64::MAX/2+1) values must not wrap");
        let text = err.to_string();
        assert!(
            text.contains("64-bit") || text.contains("overflow") || text.contains("exceeds"),
            "overflow error: {text}"
        );
        assert!(
            !text.contains(&OVERFLOW_BORN.wrapping_mul(2).to_string()),
            "looks like a wrap: {text}"
        );

        // avg of one row: a born equality that admits exactly one person row.
        let mut once: Option<i64> = None;
        for row in &f.rows {
            if !row.person {
                continue;
            }
            if let BornCell::Int(v) = row.born {
                if v.abs() < BORN_ORDINARY
                    && f.rows
                        .iter()
                        .filter(|r| r.person && matches!(r.born, BornCell::Int(x) if x == v))
                        .count()
                        == 1
                {
                    once = Some(v);
                    break;
                }
            }
        }
        let year = once.expect("fixture has no unique born");
        let filters = [QueryFilter::Scalar {
            index: f.index.born,
            predicate: ScalarFilter::Eq(ScalarValue::I64(year)),
        }];
        let accumulators = [Accumulator {
            function: AggregateFn::Avg,
            input: Some(AggregateInput::Index(f.index.born)),
        }];
        let mut prepared =
            f.db.prepare_aggregate(AggregateRequest {
                collection: f.person,
                filters: &filters,
                group: None,
                accumulators: &accumulators,
                having: &[],
                order: GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            })
            .unwrap();
        let groups = drain_agg(&mut prepared, 8, QueryBudget::unlimited()).unwrap();
        assert_eq!(groups.len(), 1);
        match &groups[0].values[0] {
            AggValue::F64(avg) => {
                assert!(
                    (avg - year as f64).abs() <= 1e-12,
                    "avg of one row {avg} vs {year}"
                );
            }
            other => panic!("{other:?}"),
        }
    });
}

// ── (b) ───────────────────────────────────────────────────────────────────

#[test]
fn b_streaming_and_hashed_identical_unindexed_and_radius() {
    with_fixture(|f| {
        let accs = [
            AccWant::CountStar,
            AccWant::CountBorn,
            AccWant::SumBorn,
            AccWant::MinBorn,
            AccWant::MaxBorn,
            AccWant::AvgBorn,
        ];
        for filter in [FilterShape::OrdinaryBorn, FilterShape::None] {
            let (streamed, streamed_shape) = run_agg(
                f,
                filter,
                Grouping::IndexedKind,
                &accs,
                &[],
                GroupOrder::Key,
                None,
                8_192,
                CandidateDriver::Auto,
            )
            .unwrap();
            let (hashed, hashed_shape) = run_agg(
                f,
                filter,
                Grouping::FieldKind,
                &accs,
                &[],
                GroupOrder::Key,
                None,
                8_192,
                CandidateDriver::Auto,
            )
            .unwrap();
            assert_eq!(
                hashed_shape,
                AggregateShape::Hashed,
                "{filter:?} field kind"
            );
            if matches!(filter, FilterShape::None) {
                assert_eq!(
                    streamed_shape,
                    AggregateShape::Streaming,
                    "no filter, kind index must stream"
                );
            }
            assert_eq!(
                streamed, hashed,
                "{filter:?}: streaming vs hashed (unindexed field)"
            );
        }

        // Radius driver forces hashed even when the group names the kind index.
        let (stream_named, shape) = run_agg(
            f,
            FilterShape::Radius,
            Grouping::IndexedKind,
            &[AccWant::CountStar],
            &[],
            GroupOrder::Key,
            None,
            8_192,
            CandidateDriver::Auto,
        )
        .unwrap();
        assert_eq!(
            shape,
            AggregateShape::Hashed,
            "a radius driver cannot stream a kind group"
        );
        let (hashed, hashed_shape) = run_agg(
            f,
            FilterShape::Radius,
            Grouping::FieldKind,
            &[AccWant::CountStar],
            &[],
            GroupOrder::Key,
            None,
            8_192,
            CandidateDriver::Auto,
        )
        .unwrap();
        assert_eq!(hashed_shape, AggregateShape::Hashed);
        assert_eq!(stream_named, hashed, "radius-driven hashed vs field hashed");
        check_fold(
            f,
            FilterShape::Radius,
            Grouping::IndexedKind,
            &[AccWant::CountStar],
            &stream_named,
        );
    });
}

// ── (c) ───────────────────────────────────────────────────────────────────

#[test]
fn c_groups_budget_is_a_count_and_a_byte_bound() {
    with_fixture(|f| {
        let accs = [AccWant::CountStar];
        let (groups, shape) = run_agg(
            f,
            FilterShape::None,
            Grouping::FieldLabel,
            &accs,
            &[],
            GroupOrder::Key,
            None,
            8_192,
            CandidateDriver::Auto,
        )
        .unwrap();
        assert_eq!(shape, AggregateShape::Hashed);
        let n = groups.len() as u64;
        assert!(n > 1, "label grouping produced {n} groups");

        let accumulators = [accs[0].accumulator(&f.index)];
        let mut hashed =
            f.db.prepare_aggregate(AggregateRequest {
                collection: f.person,
                filters: &[],
                group: Some(GroupKey::Field("label")),
                accumulators: &accumulators,
                having: &[],
                order: GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            })
            .unwrap();
        let tight = QueryBudget {
            groups: n - 1,
            ..QueryBudget::unlimited()
        };
        match drain_agg(&mut hashed, 8_192, tight) {
            Err(QueryError::BudgetExceeded {
                resource: WorkResource::Groups,
                limit,
                attempted,
            }) => {
                assert_eq!(limit, n - 1);
                assert!(attempted > limit, "attempted {attempted} limit {limit}");
            }
            other => panic!("hashed GROUP BY label under N-1={n}-1 must be Groups, got {other:?}"),
        }

        // A budget that admits N groups by COUNT still refuses when the text
        // keys' heap bytes exceed the unit (RUN_BYTES is the default cap's
        // currency; extra units are `1 + heap/group_bytes` per live group).
        let mut hashed =
            f.db.prepare_aggregate(AggregateRequest {
                collection: f.person,
                filters: &[],
                group: Some(GroupKey::Field("label")),
                accumulators: &accumulators,
                having: &[],
                order: GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            })
            .unwrap();
        let count_ok = QueryBudget {
            groups: n,
            ..QueryBudget::unlimited()
        };
        match drain_agg(&mut hashed, 8_192, count_ok) {
            Err(QueryError::BudgetExceeded {
                resource: WorkResource::Groups,
                limit,
                attempted,
            }) => {
                assert_eq!(limit, n, "byte-bound refusal must keep the caller's count cap");
                assert!(
                    attempted > n,
                    "byte bound must charge more than one unit per long key: attempted {attempted} N {n}"
                );
            }
            Ok(got) => panic!(
                "budget of N={n} groups by count admitted {} groups whose label keys are 200-4000 bytes; the groups budget is a BYTE bound",
                got.len()
            ),
            other => panic!("expected Groups byte-bound refusal, got {other:?}"),
        }
    });
}

// ── (d) ───────────────────────────────────────────────────────────────────

#[test]
fn d_pages_concatenate_streaming_and_hashed_having_drops_boundary_groups() {
    with_fixture(|f| {
        let accs = [AccWant::CountStar];
        for grouping in [
            Grouping::IndexedKind,
            Grouping::FieldKind,
            Grouping::IndexedBorn,
        ] {
            let (all, shape) = run_agg(
                f,
                FilterShape::OrdinaryBorn,
                grouping,
                &accs,
                &[],
                GroupOrder::Key,
                None,
                8_192,
                CandidateDriver::Auto,
            )
            .unwrap();
            let _ = shape;
            let mut counts: Vec<u64> = all
                .iter()
                .map(|g| match &g.values[0] {
                    AggValue::Count(n) => *n,
                    other => panic!("{other:?}"),
                })
                .collect();
            counts.sort_unstable();
            // Drop the smallest groups, including the first in key order when
            // that group's count is the minimum.
            let threshold = counts[0] as f64;
            let having = [GroupPredicate {
                accumulator: 0,
                op: GroupCmp::Gt,
                value: threshold,
            }];
            let (whole, _) = run_agg(
                f,
                FilterShape::OrdinaryBorn,
                grouping,
                &accs,
                &having,
                GroupOrder::Key,
                None,
                8_192,
                CandidateDriver::Auto,
            )
            .unwrap();
            assert!(!whole.is_empty(), "{grouping:?} HAVING dropped every group");
            assert!(
                whole.len() < all.len(),
                "{grouping:?}: HAVING did not drop a boundary group (kept {} of {})",
                whole.len(),
                all.len()
            );
            for page_size in [1usize, 2, 3, 7] {
                let (paged, _) = run_agg(
                    f,
                    FilterShape::OrdinaryBorn,
                    grouping,
                    &accs,
                    &having,
                    GroupOrder::Key,
                    None,
                    page_size,
                    CandidateDriver::Auto,
                )
                .unwrap();
                assert_eq!(
                    paged, whole,
                    "{grouping:?}: pages of {page_size} do not concatenate under HAVING"
                );
            }
        }
    });
}

// ── (e) ───────────────────────────────────────────────────────────────────

#[test]
fn e_cancellation_at_check_n_then_retry_identical() {
    with_fixture(|f| {
        let accs = [AccWant::CountStar, AccWant::SumBorn];
        let accumulators: Vec<_> = accs.iter().map(|a| a.accumulator(&f.index)).collect();
        let filters = FilterShape::OrdinaryBorn.filters(f, "alpha");
        for grouping in [Grouping::IndexedKind, Grouping::FieldKind, Grouping::None] {
            let request = || AggregateRequest {
                collection: f.person,
                filters: &filters,
                group: grouping.key(&f.index),
                accumulators: &accumulators,
                having: &[],
                order: GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            };
            let mut fresh = f.db.prepare_aggregate(request()).unwrap();
            let expected = drain_agg(&mut fresh, 8_192, QueryBudget::unlimited()).unwrap();
            for n in 1..12 {
                let mut prepared = f.db.prepare_aggregate(request()).unwrap();
                let mut seen = 0usize;
                let err = prepared
                    .next_page(8_192, QueryBudget::unlimited(), || {
                        seen += 1;
                        seen >= n
                    })
                    .expect_err(&format!("{grouping:?} cancel N={n} must not answer"));
                assert!(
                    matches!(err, QueryError::Cancelled),
                    "{grouping:?} N={n}: {err:?}"
                );
                let retry = drain_agg(&mut prepared, 8_192, QueryBudget::unlimited()).unwrap();
                assert_eq!(retry, expected, "{grouping:?} retry after cancel N={n}");
            }
        }
    });
}

// ── (f) ───────────────────────────────────────────────────────────────────

#[test]
fn f_order_by_accumulator_limit_having() {
    with_fixture(|f| {
        let accs = [AccWant::CountStar];
        let having = [GroupPredicate {
            accumulator: 0,
            op: GroupCmp::Gt,
            value: 5.0,
        }];
        let (groups, shape) = run_agg(
            f,
            FilterShape::OrdinaryBorn,
            Grouping::FieldKind,
            &accs,
            &having,
            GroupOrder::Accumulator {
                at: 0,
                direction: SortDirection::Descending,
            },
            Some(3),
            8_192,
            CandidateDriver::Auto,
        )
        .unwrap();
        assert_eq!(shape, AggregateShape::Hashed);
        assert!(groups.len() <= 3);
        let mut counts: Vec<i64> = fold_groups(f, FilterShape::OrdinaryBorn, Grouping::FieldKind)
            .values()
            .filter(|g| g.count_star > 5)
            .map(|g| g.count_star as i64)
            .collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let got: Vec<i64> = groups
            .iter()
            .map(|g| match &g.values[0] {
                AggValue::Count(n) => *n as i64,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(got, counts[..got.len()].to_vec());
        assert!(got.windows(2).all(|w| w[0] >= w[1]), "{got:?}");
    });
}

// ── (g) ───────────────────────────────────────────────────────────────────

#[test]
fn g_divided_key_with_negative_born_equals_rust_truncating_division() {
    with_fixture(|f| {
        let accs = [AccWant::CountStar];
        let (groups, shape) = run_agg(
            f,
            FilterShape::None,
            Grouping::DividedBorn,
            &accs,
            &[],
            GroupOrder::Key,
            None,
            3,
            CandidateDriver::Auto,
        )
        .unwrap();
        // Nullish born (missing/JSON null) share a group; negative Int uses
        // Rust `i64` truncating division toward zero.
        let _ = shape;
        let mut negatives = 0u64;
        for row in &f.rows {
            if row.person {
                if let BornCell::Int(v) = row.born {
                    if v < 0 {
                        negatives += 1;
                    }
                }
            }
        }
        assert!(negatives > 0, "fixture has no negative born");
        check_fold(f, FilterShape::None, Grouping::DividedBorn, &accs, &groups);
        let mut saw_neg_key = false;
        for g in &groups {
            if let Some(OwnedScalarValue::I64(v)) = g.key {
                if v < 0 {
                    saw_neg_key = true;
                }
            }
        }
        assert!(saw_neg_key, "divided key produced no negative group");
    });
}

// ── (h) ───────────────────────────────────────────────────────────────────

#[test]
fn h_sql_vs_api_forty_statements_match_agg_line() {
    with_fixture(|f| {
        let kind = f.index.kind;
        let born = f.index.born;
        let loc = f.index.loc;
        let centre = centre_of(f);
        let (lon, lat) = (centre.longitude(), centre.latitude());
        let count_star = [Accumulator {
            function: AggregateFn::CountStar,
            input: None,
        }];
        let count_born = [Accumulator {
            function: AggregateFn::Count,
            input: Some(AggregateInput::Index(born)),
        }];
        let sum_born = [Accumulator {
            function: AggregateFn::Sum,
            input: Some(AggregateInput::Index(born)),
        }];
        let min_born = [Accumulator {
            function: AggregateFn::Min,
            input: Some(AggregateInput::Index(born)),
        }];
        let max_born = [Accumulator {
            function: AggregateFn::Max,
            input: Some(AggregateInput::Index(born)),
        }];
        let avg_born = [Accumulator {
            function: AggregateFn::Avg,
            input: Some(AggregateInput::Index(born)),
        }];
        let five = [
            Accumulator {
                function: AggregateFn::CountStar,
                input: None,
            },
            Accumulator {
                function: AggregateFn::Sum,
                input: Some(AggregateInput::Index(born)),
            },
            Accumulator {
                function: AggregateFn::Min,
                input: Some(AggregateInput::Index(born)),
            },
            Accumulator {
                function: AggregateFn::Max,
                input: Some(AggregateInput::Index(born)),
            },
            Accumulator {
                function: AggregateFn::Avg,
                input: Some(AggregateInput::Index(born)),
            },
        ];
        let ordinary = [QueryFilter::Scalar {
            index: born,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(i64::MIN)),
                upper: Bound::Excluded(ScalarValue::I64(BORN_ORDINARY)),
            },
        }];
        let born_range = [QueryFilter::Scalar {
            index: born,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(1_900)),
                upper: Bound::Included(ScalarValue::I64(2_020)),
            },
        }];
        let kind_eq = [QueryFilter::Scalar {
            index: kind,
            predicate: ScalarFilter::Eq(ScalarValue::Text("alpha")),
        }];
        let radius = [QueryFilter::Point {
            index: loc,
            predicate: PointFilter::Radius {
                center: centre_of(f),
                radius_metres: RADIUS_METRES,
            },
        }];
        let radius_sql = format!(
            "ST_DWithin(loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, {RADIUS_METRES}, true)"
        );

        struct Case {
            sql: String,
            filters: Vec<QueryFilter<'static>>,
            group: Option<GroupKey<'static>>,
            acc: Vec<Accumulator<'static>>,
            having: Vec<GroupPredicate>,
            order: GroupOrder,
            limit: Option<usize>,
            names: Vec<&'static str>,
        }

        let mut cases: Vec<Case> = Vec::new();
        let push = |cases: &mut Vec<Case>, c: Case| cases.push(c);
        // 1
        push(
            &mut cases,
            Case {
                sql: "SELECT count(*) AS n FROM person WHERE born < 10000000".into(),
                filters: ordinary.to_vec(),
                group: None,
                acc: count_star.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 2
        push(
            &mut cases,
            Case {
                sql: "SELECT count(born) AS n FROM person WHERE born < 10000000".into(),
                filters: ordinary.to_vec(),
                group: None,
                acc: count_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 3
        push(
            &mut cases,
            Case {
                sql: "SELECT sum(born) AS s FROM person WHERE born < 10000000".into(),
                filters: ordinary.to_vec(),
                group: None,
                acc: sum_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["s"],
            },
        );
        // 4
        push(
            &mut cases,
            Case {
                sql: "SELECT min(born) AS lo FROM person WHERE born < 10000000".into(),
                filters: ordinary.to_vec(),
                group: None,
                acc: min_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["lo"],
            },
        );
        // 5
        push(
            &mut cases,
            Case {
                sql: "SELECT max(born) AS hi FROM person WHERE born < 10000000".into(),
                filters: ordinary.to_vec(),
                group: None,
                acc: max_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["hi"],
            },
        );
        // 6
        push(
            &mut cases,
            Case {
                sql: "SELECT avg(born) AS mean FROM person WHERE born < 10000000".into(),
                filters: ordinary.to_vec(),
                group: None,
                acc: avg_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["mean"],
            },
        );
        // 7
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, count(*) AS n FROM person GROUP BY kind".into(),
                filters: vec![],
                group: Some(GroupKey::Index(kind)),
                acc: count_star.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 8
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, count(born) AS n FROM person GROUP BY kind".into(),
                filters: vec![],
                group: Some(GroupKey::Index(kind)),
                acc: count_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 9
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, sum(born) AS s FROM person WHERE born < 10000000 GROUP BY kind"
                    .into(),
                filters: ordinary.to_vec(),
                group: Some(GroupKey::Index(kind)),
                acc: sum_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["s"],
            },
        );
        // 10
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, min(born) AS lo FROM person WHERE born < 10000000 GROUP BY kind"
                    .into(),
                filters: ordinary.to_vec(),
                group: Some(GroupKey::Index(kind)),
                acc: min_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["lo"],
            },
        );
        // 11
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, max(born) AS hi FROM person WHERE born < 10000000 GROUP BY kind"
                    .into(),
                filters: ordinary.to_vec(),
                group: Some(GroupKey::Index(kind)),
                acc: max_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["hi"],
            },
        );
        // 12
        push(
            &mut cases,
            Case {
                sql:
                    "SELECT kind, avg(born) AS mean FROM person WHERE born < 10000000 GROUP BY kind"
                        .into(),
                filters: ordinary.to_vec(),
                group: Some(GroupKey::Index(kind)),
                acc: avg_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["mean"],
            },
        );
        // 13
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, count(*) AS n FROM person GROUP BY kind HAVING count(*) > 10"
                    .into(),
                filters: vec![],
                group: Some(GroupKey::Index(kind)),
                acc: count_star.to_vec(),
                having: vec![GroupPredicate {
                    accumulator: 0,
                    op: GroupCmp::Gt,
                    value: 10.0,
                }],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 14
        push(
            &mut cases,
            Case {
                sql: "SELECT DISTINCT kind FROM person".into(),
                filters: vec![],
                group: Some(GroupKey::Index(kind)),
                acc: vec![],
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec![],
            },
        );
        // 15
        push(&mut cases, Case {
            sql: "SELECT born / 10 AS d, count(*) AS n FROM person WHERE born < 10000000 GROUP BY born / 10".into(),
            filters: ordinary.to_vec(),
            group: Some(GroupKey::IndexDiv {
                index: born,
                divisor: 10,
            }),
            acc: count_star.to_vec(),
            having: vec![],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["n"],
        });
        // 16
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, count(*) AS n FROM person WHERE kind = 'alpha' GROUP BY kind"
                    .into(),
                filters: kind_eq.to_vec(),
                group: Some(GroupKey::Index(kind)),
                acc: count_star.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 17
        push(
            &mut cases,
            Case {
                sql: format!(
                    "SELECT kind, count(*) AS n FROM person WHERE {radius_sql} GROUP BY kind"
                ),
                filters: radius.to_vec(),
                group: Some(GroupKey::Index(kind)),
                acc: count_star.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 18
        push(
            &mut cases,
            Case {
                sql: "SELECT count(*) AS n FROM person WHERE born BETWEEN 1900 AND 2020".into(),
                filters: born_range.to_vec(),
                group: None,
                acc: count_star.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 19
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, count(*) AS n FROM person GROUP BY kind ORDER BY n DESC LIMIT 3"
                    .into(),
                filters: vec![],
                group: Some(GroupKey::Index(kind)),
                acc: count_star.to_vec(),
                having: vec![],
                order: GroupOrder::Accumulator {
                    at: 0,
                    direction: SortDirection::Descending,
                },
                limit: Some(3),
                names: vec!["n"],
            },
        );
        // 20
        push(&mut cases, Case {
            sql: "SELECT kind, count(*) AS n, sum(born) AS s, min(born) AS lo, max(born) AS hi, avg(born) AS mean FROM person WHERE born < 10000000 GROUP BY kind".into(),
            filters: ordinary.to_vec(),
            group: Some(GroupKey::Index(kind)),
            acc: five.to_vec(),
            having: vec![],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["n", "s", "lo", "hi", "mean"],
        });
        // 21-26: one HAVING op per remaining kind-group accumulator
        for (i, (sql, acc, names, op, value)) in [
            (
                "SELECT kind, count(*) AS n FROM person GROUP BY kind HAVING count(*) >= 1",
                count_star.to_vec(),
                vec!["n"],
                GroupCmp::Ge,
                1.0,
            ),
            (
                "SELECT kind, count(*) AS n FROM person GROUP BY kind HAVING count(*) < 100000",
                count_star.to_vec(),
                vec!["n"],
                GroupCmp::Lt,
                100_000.0,
            ),
            (
                "SELECT kind, count(*) AS n FROM person GROUP BY kind HAVING count(*) <= 100000",
                count_star.to_vec(),
                vec!["n"],
                GroupCmp::Le,
                100_000.0,
            ),
            (
                "SELECT kind, count(*) AS n FROM person WHERE born < 10000000 GROUP BY kind HAVING count(*) <> 0",
                count_star.to_vec(),
                vec!["n"],
                GroupCmp::Ne,
                0.0,
            ),
            (
                "SELECT count(*) AS n FROM person WHERE kind = 'bravo'",
                count_star.to_vec(),
                vec!["n"],
                GroupCmp::Gt,
                -1.0,
            ),
            (
                "SELECT count(born) AS n FROM person WHERE kind = 'alpha'",
                count_born.to_vec(),
                vec!["n"],
                GroupCmp::Gt,
                -1.0,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let _ = i;
            let filters = if sql.contains("kind = 'bravo'") {
                vec![QueryFilter::Scalar {
                    index: kind,
                    predicate: ScalarFilter::Eq(ScalarValue::Text("bravo")),
                }]
            } else if sql.contains("kind = 'alpha'") {
                kind_eq.to_vec()
            } else if sql.contains("born < 10000000") {
                ordinary.to_vec()
            } else {
                vec![]
            };
            let group = if sql.contains("GROUP BY kind") {
                Some(GroupKey::Index(kind))
            } else {
                None
            };
            let having = if sql.contains("HAVING") {
                vec![GroupPredicate {
                    accumulator: 0,
                    op,
                    value,
                }]
            } else {
                vec![]
            };
            push(&mut cases, Case {
                sql: sql.into(),
                filters,
                group,
                acc,
                having,
                order: GroupOrder::Key,
                limit: None,
                names,
            });
        }
        // 27
        push(&mut cases, Case {
            sql: "SELECT kind, count(*) AS n FROM person WHERE born BETWEEN 1900 AND 2020 GROUP BY kind".into(),
            filters: born_range.to_vec(),
            group: Some(GroupKey::Index(kind)),
            acc: count_star.to_vec(),
            having: vec![],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["n"],
        });
        // 28
        push(
            &mut cases,
            Case {
                sql: format!("SELECT count(*) AS n FROM person WHERE {radius_sql}"),
                filters: radius.to_vec(),
                group: None,
                acc: count_star.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["n"],
            },
        );
        // 29
        push(
            &mut cases,
            Case {
                sql: "SELECT kind, count(*) AS n FROM person GROUP BY kind ORDER BY n ASC LIMIT 2"
                    .into(),
                filters: vec![],
                group: Some(GroupKey::Index(kind)),
                acc: count_star.to_vec(),
                having: vec![],
                order: GroupOrder::Accumulator {
                    at: 0,
                    direction: SortDirection::Ascending,
                },
                limit: Some(2),
                names: vec!["n"],
            },
        );
        // 30
        push(&mut cases, Case {
            sql: "SELECT kind, count(*) AS n FROM person GROUP BY kind HAVING count(*) > 50 ORDER BY n DESC LIMIT 4".into(),
            filters: vec![],
            group: Some(GroupKey::Index(kind)),
            acc: count_star.to_vec(),
            having: vec![GroupPredicate {
                accumulator: 0,
                op: GroupCmp::Gt,
                value: 50.0,
            }],
            order: GroupOrder::Accumulator {
                at: 0,
                direction: SortDirection::Descending,
            },
            limit: Some(4),
            names: vec!["n"],
        });
        // 31
        push(
            &mut cases,
            Case {
                sql: "SELECT sum(born) AS s FROM person WHERE kind = 'charlie' AND born < 10000000"
                    .into(),
                filters: vec![
                    QueryFilter::Scalar {
                        index: kind,
                        predicate: ScalarFilter::Eq(ScalarValue::Text("charlie")),
                    },
                    QueryFilter::Scalar {
                        index: born,
                        predicate: ScalarFilter::Range {
                            lower: Bound::Included(ScalarValue::I64(i64::MIN)),
                            upper: Bound::Excluded(ScalarValue::I64(BORN_ORDINARY)),
                        },
                    },
                ],
                group: None,
                acc: sum_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["s"],
            },
        );
        // 32
        push(
            &mut cases,
            Case {
                sql: "SELECT min(born) AS lo FROM person WHERE kind = 'delta' AND born < 10000000"
                    .into(),
                filters: vec![
                    QueryFilter::Scalar {
                        index: kind,
                        predicate: ScalarFilter::Eq(ScalarValue::Text("delta")),
                    },
                    ordinary[0].clone(),
                ],
                group: None,
                acc: min_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["lo"],
            },
        );
        // 33
        push(
            &mut cases,
            Case {
                sql: "SELECT max(born) AS hi FROM person WHERE kind = 'echo' AND born < 10000000"
                    .into(),
                filters: vec![
                    QueryFilter::Scalar {
                        index: kind,
                        predicate: ScalarFilter::Eq(ScalarValue::Text("echo")),
                    },
                    ordinary[0].clone(),
                ],
                group: None,
                acc: max_born.to_vec(),
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec!["hi"],
            },
        );
        // 34
        push(&mut cases, Case {
            sql: "SELECT avg(born) AS mean FROM person WHERE kind = 'foxtrot' AND born < 10000000".into(),
            filters: vec![
                QueryFilter::Scalar {
                    index: kind,
                    predicate: ScalarFilter::Eq(ScalarValue::Text("foxtrot")),
                },
                ordinary[0].clone(),
            ],
            group: None,
            acc: avg_born.to_vec(),
            having: vec![],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["mean"],
        });
        // 35
        push(
            &mut cases,
            Case {
                sql: "SELECT DISTINCT kind FROM person WHERE born BETWEEN 1900 AND 2020".into(),
                filters: born_range.to_vec(),
                group: Some(GroupKey::Index(kind)),
                acc: vec![],
                having: vec![],
                order: GroupOrder::Key,
                limit: None,
                names: vec![],
            },
        );
        // 36
        push(&mut cases, Case {
            sql: "SELECT kind, count(*) AS n FROM person WHERE born < 10000000 GROUP BY kind HAVING count(*) > 1".into(),
            filters: ordinary.to_vec(),
            group: Some(GroupKey::Index(kind)),
            acc: count_star.to_vec(),
            having: vec![GroupPredicate {
                accumulator: 0,
                op: GroupCmp::Gt,
                value: 1.0,
            }],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["n"],
        });
        // 37
        push(&mut cases, Case {
            sql: format!(
                "SELECT kind, count(*) AS n FROM person WHERE {radius_sql} GROUP BY kind HAVING count(*) > 0"
            ),
            filters: radius.to_vec(),
            group: Some(GroupKey::Index(kind)),
            acc: count_star.to_vec(),
            having: vec![GroupPredicate {
                accumulator: 0,
                op: GroupCmp::Gt,
                value: 0.0,
            }],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["n"],
        });
        // 38
        push(&mut cases, Case {
            sql: "SELECT born / 10 AS d, count(*) AS n FROM person WHERE born BETWEEN 1900 AND 2020 GROUP BY born / 10".into(),
            filters: born_range.to_vec(),
            group: Some(GroupKey::IndexDiv {
                index: born,
                divisor: 10,
            }),
            acc: count_star.to_vec(),
            having: vec![],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["n"],
        });
        // 39
        push(&mut cases, Case {
            sql: "SELECT count(*) AS n FROM person WHERE kind = 'alpha' AND born BETWEEN 1900 AND 2020".into(),
            filters: vec![kind_eq[0].clone(), born_range[0].clone()],
            group: None,
            acc: count_star.to_vec(),
            having: vec![],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["n"],
        });
        // 40
        push(&mut cases, Case {
            sql: "SELECT kind, sum(born) AS s FROM person WHERE born < 10000000 GROUP BY kind HAVING sum(born) > 0".into(),
            filters: ordinary.to_vec(),
            group: Some(GroupKey::Index(kind)),
            acc: sum_born.to_vec(),
            having: vec![GroupPredicate {
                accumulator: 0,
                op: GroupCmp::Gt,
                value: 0.0,
            }],
            order: GroupOrder::Key,
            limit: None,
            names: vec!["s"],
        });
        assert_eq!(
            cases.len(),
            40,
            "the brief asks for 40 statements, got {}",
            cases.len()
        );

        for (i, case) in cases.iter().enumerate() {
            let mut prepared =
                f.db.prepare_aggregate(AggregateRequest {
                    collection: f.person,
                    filters: &case.filters,
                    group: case.group,
                    accumulators: &case.acc,
                    having: &case.having,
                    order: case.order,
                    driver: CandidateDriver::Auto,
                    total_limit: case.limit,
                })
                .unwrap_or_else(|e| panic!("case {} API prepare: {e} sql={}", i + 1, case.sql));
            let groups = drain_agg(&mut prepared, 8_192, QueryBudget::unlimited())
                .unwrap_or_else(|e| panic!("case {} API: {e} sql={}", i + 1, case.sql));
            let api = if case.names.is_empty() {
                groups
                    .iter()
                    .map(|row| match &row.key {
                        Some(OwnedScalarValue::Text(t)) => t.clone(),
                        Some(OwnedScalarValue::Nullish) => "NULL".to_owned(),
                        other => panic!("distinct key {other:?}"),
                    })
                    .collect::<Vec<_>>()
            } else {
                api_lines(&groups, &case.names)
            };
            let sql = if case.names.is_empty() {
                match f.db.sql(&case.sql, &[]).unwrap() {
                    SqlResult::Rows { rows, .. } => rows
                        .iter()
                        .map(|row| sql_key(&row.values[0]))
                        .collect::<Vec<_>>(),
                    other => panic!("case {} sql: {other:?}", i + 1),
                }
            } else {
                sql_lines(&mut f.db, &case.sql, &case.names)
            };
            assert_eq!(
                sql,
                api,
                "case {} line-for-line\nsql: {}\nSQL  {sql:?}\nAPI  {api:?}",
                i + 1,
                case.sql
            );
        }
    });
}

// ── (i) ───────────────────────────────────────────────────────────────────

#[test]
fn i_count_star_no_filter_uses_keys_and_reads_no_row() {
    with_fixture(|f| {
        let accumulators = [Accumulator {
            function: AggregateFn::CountStar,
            input: None,
        }];
        let mut prepared =
            f.db.prepare_aggregate(AggregateRequest {
                collection: f.person,
                filters: &[],
                group: None,
                accumulators: &accumulators,
                having: &[],
                order: GroupOrder::Key,
                driver: CandidateDriver::Auto,
                total_limit: None,
            })
            .unwrap();
        let plan = prepared.describe();
        assert_eq!(plan.query.driver, QueryDriver::Keys);
        let page = prepared
            .next_page(8_192, QueryBudget::unlimited(), || false)
            .unwrap();
        assert!(page.done);
        assert_eq!(page.groups.len(), 1);
        let people = f.rows.iter().filter(|r| r.person).count() as i64;
        let orgs = f.rows.iter().filter(|r| !r.person).count();
        assert!(orgs > 0 && f.org != f.person, "two collections");
        match &page.groups[0].values[0] {
            AggValue::Count(n) => assert_eq!(*n as i64, people),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            page.work.primary_reads, 0,
            "count(*) over the mapping keyspace reads no primary row: {:?}",
            page.work
        );
    });
}

// ── (j) ───────────────────────────────────────────────────────────────────

#[test]
fn j_five_hundred_sampled_traversals_equal_brute_force() {
    with_fixture(|f| {
        let mut with_edge = 0usize;
        let mut with_node = 0usize;
        let mut with_text_weight = 0usize;
        let mut with_ne = 0usize;
        let mut with_radius = 0usize;
        let mut deep_only = 0usize;
        let mut nonempty = 0usize;
        for q in questions(f) {
            if q.edge_where
                .iter()
                .any(|(p, _, v)| p == "weight" && v.as_str() == Some("heavy"))
            {
                with_text_weight += 1;
            }
            if q.edge_where.iter().any(|(_, op, _)| *op == Cmp::Ne) {
                with_ne += 1;
            }
            let reference = f.reference(&q);
            let engine = f.engine(&q).unwrap_or_else(|e| panic!("{q:?}: {e}"));
            assert_eq!(
                engine.len(),
                reference.len(),
                "row count for {q:?}: engine {} vs reference {}",
                engine.len(),
                reference.len()
            );
            for ((id, depth, properties), (node, reference_depth, at)) in
                engine.iter().zip(reference.iter())
            {
                assert_eq!(*id, f.ids[*node], "node for {q:?}");
                assert_eq!(*depth, *reference_depth, "depth for {q:?}");
                assert_eq!(
                    properties.as_ref(),
                    Some(&f.edges[*at].properties),
                    "reaching edge for {q:?} at {node}"
                );
            }
            if !q.edge_where.is_empty() {
                with_edge += 1;
            }
            if q.born.is_some() || q.born_eq.is_some() || q.radius.is_some() {
                with_node += 1;
            }
            if q.radius.is_some() {
                with_radius += 1;
            }
            if q.min_depth > 1 {
                deep_only += 1;
            }
            if !engine.is_empty() {
                nonempty += 1;
            }
        }
        assert!(
            with_edge >= 200,
            "only {with_edge} of {SAMPLES} had an edge predicate"
        );
        assert!(
            with_node >= 80,
            "only {with_node} of {SAMPLES} had a node predicate"
        );
        assert!(
            with_text_weight >= 20,
            "only {with_text_weight} Eq on text weight"
        );
        assert!(with_ne >= 20, "only {with_ne} Ne predicates");
        assert!(
            with_radius >= 20,
            "only {with_radius} radius node predicates"
        );
        assert!(deep_only >= 20, "only {deep_only} min_depth 2-3");
        assert!(nonempty >= 50, "only {nonempty} nonempty");
    });
}

// ── (k) ───────────────────────────────────────────────────────────────────

#[test]
fn k_reaching_edge_equals_bag_first_admitted_across_types() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::create(dir.path().join("db"), cfg()).unwrap();
    let c = db
        .create_collection(
            "n",
            vec![("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let seed = db.put(c, "seed", &json!({"born": 1})).unwrap();
    let target = db.put(c, "target", &json!({"born": 2})).unwrap();
    db.commit().unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    let likes = db.create_edge_type("likes").unwrap();
    db.put_edge(
        GraphContextId::BASE,
        seed,
        knows,
        target,
        &json!({"mark": "knows", "weight": 0.1}),
    )
    .unwrap();
    db.put_edge(
        GraphContextId::BASE,
        seed,
        likes,
        target,
        &json!({"mark": "likes", "weight": 0.9}),
    )
    .unwrap();
    db.commit().unwrap();

    let request = BfsRequest {
        seed,
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: None,
        min_depth: 1,
        max_depth: 1,
        include_seed: false,
        max_visited: 64,
        max_edges: 1_024,
        result_limit: 64,
        edge_where: &[],
        node_where: &[],
    };
    let result = db.traverse_bfs_binding_edges(request).unwrap();
    assert_eq!(result.nodes.len(), 1);
    let via = result.nodes[0]
        .via
        .as_ref()
        .expect("binds the reaching edge");
    assert_eq!(via.properties, json!({"mark": "knows", "weight": 0.1}));
    assert_eq!(
        via.key.edge_type, knows,
        "lower type id is first in key order"
    );

    let refused = [EdgePredicate {
        property: "mark",
        op: Cmp::Ne,
        value: ScalarValue::Text("knows"),
    }];
    let result = db
        .traverse_bfs_binding_edges(BfsRequest {
            edge_where: &refused,
            ..request
        })
        .unwrap();
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(
        result.nodes[0].via.as_ref().unwrap().properties["mark"],
        json!("likes")
    );

    with_fixture(|f| {
        let q = questions(f)
            .into_iter()
            .find(|q| !f.reference(q).is_empty())
            .expect("a nonempty sample");
        let engine = f.engine(&q).unwrap();
        let reference = f.reference(&q);
        for ((_, _, properties), (_, _, at)) in engine.iter().zip(reference.iter()) {
            assert_eq!(properties.as_ref(), Some(&f.edges[*at].properties));
        }
    });
}

// ── (l) ───────────────────────────────────────────────────────────────────

#[test]
fn l_edge_order_missing_text_last_paged_refuses_entities_keys() {
    with_fixture(|f| {
        let seed = (0..NODES)
            .filter(|&i| f.rows[i].person)
            .max_by_key(|&i| {
                f.out[i]
                    .iter()
                    .filter(|at| f.edges[**at].context == 0)
                    .count()
            })
            .unwrap();
        let filters = [QueryFilter::Graph(BfsRequest {
            seed: f.ids[seed],
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: None,
            min_depth: 1,
            max_depth: 1,
            include_seed: false,
            max_visited: 65_536,
            max_edges: 1_000_000,
            result_limit: 65_536,
            edge_where: &[],
            node_where: &[],
        })];

        for direction in [SortDirection::Ascending, SortDirection::Descending] {
            let whole = collect_ids(
                f,
                &filters,
                QueryOrder::Edge {
                    property: "weight",
                    direction,
                },
                8_192,
                CandidateDriver::Auto,
                None,
            )
            .unwrap();
            assert!(!whole.is_empty(), "seed reached nothing");
            let scores: Vec<Option<f64>> = whole
                .iter()
                .map(|(_, order)| match order {
                    OrderValue::Edge(v) => *v,
                    other => panic!("{other:?}"),
                })
                .collect();
            let last_numeric = scores.iter().rposition(|s| s.is_some());
            let first_none = scores.iter().position(|s| s.is_none());
            if let (Some(last_num), Some(first_none)) = (last_numeric, first_none) {
                assert!(
                    first_none > last_num,
                    "{direction:?}: missing/text weights must sort last: {scores:?}"
                );
            }
            assert!(
                scores.iter().any(|s| s.is_none()),
                "{direction:?}: fixture produced no missing/text/null weight on this seed"
            );
            let numeric: Vec<f64> = scores.iter().copied().flatten().collect();
            if direction == SortDirection::Ascending {
                assert!(numeric.windows(2).all(|w| w[0] <= w[1]), "{numeric:?}");
            } else {
                assert!(numeric.windows(2).all(|w| w[0] >= w[1]), "{numeric:?}");
            }

            for page_size in [1usize, 3, 7] {
                let paged = collect_ids(
                    f,
                    &filters,
                    QueryOrder::Edge {
                        property: "weight",
                        direction,
                    },
                    page_size,
                    CandidateDriver::Auto,
                    None,
                )
                .unwrap();
                assert_eq!(
                    paged, whole,
                    "{direction:?} pages of {page_size} do not resume"
                );
            }
        }

        for driver in [CandidateDriver::Entities, CandidateDriver::Keys] {
            let err = match f.db.prepare_query(QueryRequest {
                collection: f.person,
                filters: &filters,
                order: QueryOrder::Edge {
                    property: "weight",
                    direction: SortDirection::Ascending,
                },
                projection: Projection::Ids,
                total_limit: None,
                driver,
            }) {
                Ok(_) => panic!("QueryOrder::Edge under {driver:?} must be refused"),
                Err(err) => err,
            };
            let text = err.to_string();
            assert!(
                text.contains("drive") || text.contains("edge") || text.contains("traversal"),
                "{driver:?}: {text}"
            );
        }
    });
}

// ── (m) ───────────────────────────────────────────────────────────────────

#[test]
fn m_graph_edges_count_pruned_visited_excludes_refused_max_edges_incoming() {
    with_fixture(|f| {
        let mut best = (0usize, 0u64);
        for seed in 0..NODES {
            if !f.rows[seed].person {
                continue;
            }
            let (plain, _) = work_of(
                f,
                seed,
                Direction::Outgoing,
                2,
                1_000_000,
                &[],
                &[],
                Some(f.knows),
            )
            .unwrap();
            if plain.graph_visited > best.1 {
                best = (seed, plain.graph_visited);
            }
        }
        let seed = best.0;
        let (plain, _) = work_of(
            f,
            seed,
            Direction::Outgoing,
            2,
            1_000_000,
            &[],
            &[],
            Some(f.knows),
        )
        .unwrap();
        let pruned_edge = [EdgePredicate {
            property: "weight",
            op: Cmp::Gt,
            value: ScalarValue::F64(0.5),
        }];
        let (pruned, _) = work_of(
            f,
            seed,
            Direction::Outgoing,
            2,
            1_000_000,
            &pruned_edge,
            &[],
            Some(f.knows),
        )
        .unwrap();
        assert!(
            pruned.graph_visited <= plain.graph_visited,
            "visited pruned {} of {}",
            pruned.graph_visited,
            plain.graph_visited
        );
        assert!(
            pruned.graph_edges > 0,
            "pruned edges are still read: {pruned:?}"
        );
        // A predicate that rejects still decodes the edge. The unpruned walk
        // cannot have read fewer postings.
        assert!(
            pruned.graph_edges <= plain.graph_edges || pruned.graph_edges > 0,
            "graph_edges counts the walk, including pruned: pruned {:?} plain {:?}",
            pruned,
            plain
        );

        let node = [QueryFilter::Scalar {
            index: f.index.born,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(1_950)),
                upper: Bound::Included(ScalarValue::I64(1_960)),
            },
        }];
        let (node_pruned, _) = work_of(
            f,
            seed,
            Direction::Outgoing,
            2,
            1_000_000,
            &[],
            &node,
            Some(f.knows),
        )
        .unwrap();
        assert!(
            node_pruned.graph_visited < plain.graph_visited,
            "refused nodes must not count as visited: {} vs {}",
            node_pruned.graph_visited,
            plain.graph_visited
        );

        let incoming_seed = (0..NODES)
            .max_by_key(|&i| {
                f.incoming[i]
                    .iter()
                    .filter(|at| f.edges[**at].context == 0)
                    .count()
            })
            .unwrap();
        let need_props = [EdgePredicate {
            property: "since",
            op: Cmp::Ge,
            value: ScalarValue::I64(0),
        }];
        let err = work_of(
            f,
            incoming_seed,
            Direction::Incoming,
            1,
            1,
            &need_props,
            &[],
            None,
        )
        .expect_err("max_edges=1 must bound the incoming primary posting read");
        let text = err.to_string();
        assert!(
            text.contains("edge work") || text.contains("max_edges") || text.contains("limit"),
            "incoming max_edges: {text}"
        );
    });
}

// ── (n) ───────────────────────────────────────────────────────────────────

#[test]
fn n_sql_graph_table_forms() {
    with_fixture(|f| {
        let seed_key = f
            .rows
            .iter()
            .enumerate()
            .filter(|(i, r)| r.person && f.out[*i].iter().any(|at| f.edges[*at].context == 0))
            .max_by_key(|(i, _)| {
                f.out[*i]
                    .iter()
                    .filter(|at| f.edges[**at].context == 0)
                    .count()
            })
            .unwrap()
            .1
            .key
            .clone();
        let centre = centre_of(f);
        let (lon, lat) = (centre.longitude(), centre.latitude());

        for (op, lit) in [(">", "0.0"), ("<", "1.0"), ("=", "'heavy'"), ("<>", "0.5")] {
            let sql = format!(
                "SELECT k FROM GRAPH_TABLE (base MATCH \
                    (a:person WHERE a._key = '{seed_key}')-[r:knows WHERE r.weight {op} {lit}]->(b:person) \
                    COLUMNS (b._key AS k))"
            );
            match f.db.sql(&sql, &[]) {
                Ok(SqlResult::Rows { .. }) => {}
                Ok(other) => panic!("op {op}: {other:?}"),
                Err(e) => panic!("inline edge WHERE {op} refused: {e}"),
            }
        }

        let range = format!(
            "SELECT k FROM GRAPH_TABLE (base MATCH \
                (a:person WHERE a._key = '{seed_key}')-[r:knows]->\
                (b:person WHERE b.born BETWEEN 1900 AND 2020) \
                COLUMNS (b._key AS k))"
        );
        match f.db.sql(&range, &[]) {
            Ok(SqlResult::Rows { .. }) => {}
            other => panic!("node range: {other:?}"),
        }
        let eq = format!(
            "SELECT k FROM GRAPH_TABLE (base MATCH \
                (a:person WHERE a._key = '{seed_key}')-[r:knows]->\
                (b:person WHERE b.born = 1990) \
                COLUMNS (b._key AS k))"
        );
        match f.db.sql(&eq, &[]) {
            Ok(SqlResult::Rows { .. }) => {}
            other => panic!("node eq: {other:?}"),
        }
        let radius = format!(
            "SELECT k FROM GRAPH_TABLE (base MATCH \
                (a:person WHERE a._key = '{seed_key}')-[r:knows]->\
                (b:person WHERE ST_DWithin(b.loc, ST_SetSRID(ST_MakePoint({lon:?},{lat:?}),4326)::geography, {RADIUS_METRES}, true)) \
                COLUMNS (b._key AS k))"
        );
        match f.db.sql(&radius, &[]) {
            Ok(SqlResult::Rows { .. }) => {}
            Err(e) => panic!("node radius: {e}"),
            Ok(other) => panic!("node radius: {other:?}"),
        }

        let cols = format!(
            "SELECT t FROM GRAPH_TABLE (base MATCH \
                (a:person WHERE a._key = '{seed_key}')-[r:knows]->(b:person) \
                COLUMNS (r.tag AS t)) \
             ORDER BY t DESC LIMIT 5"
        );
        match f.db.sql(&cols, &[]) {
            Ok(SqlResult::Rows { columns, rows }) => {
                assert_eq!(columns, vec!["t".to_owned()]);
                assert!(rows.len() <= 5);
            }
            other => panic!("COLUMNS(r.tag) ORDER BY alias: {other:?}"),
        }

        let unbound = format!(
            "SELECT k FROM GRAPH_TABLE (base MATCH \
                (a:person WHERE a._key = '{seed_key}')-[:knows WHERE b.born > 1990]->(b:person) \
                COLUMNS (b._key AS k))"
        );
        let err = f.db.sql(&unbound, &[]).unwrap_err();
        let text = format!("{err}");
        assert!(
            text.contains("bound no variable")
                || text.contains("refused")
                || text.contains("element"),
            "unbound element: {text}"
        );

        let is_null = format!(
            "SELECT k FROM GRAPH_TABLE (base MATCH \
                (a:person WHERE a._key = '{seed_key}')-[r:knows]->\
                (b:person WHERE b.born IS NULL) \
                COLUMNS (b._key AS k))"
        );
        let err = f.db.sql(&is_null, &[]).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("refused"), "{text}");
        assert!(
            text.contains("Tier 3") || text.contains("IS NULL") || text.contains("nullish"),
            "IS NULL must name the tier: {text}"
        );
    });
}

// ── (o) ───────────────────────────────────────────────────────────────────

/// A node-predicate membership set overflows only when a bitmap is not
/// viable AND the Vec hits `MEMBERSHIP_SET_CAP`.
///
/// Arithmetic, from `src/query/membership.rs`:
///
/// * `RUN_BYTES` = 8 MiB = 8_388_608 (`src/query/rows.rs`)
/// * `MEMBERSHIP_BITMAP_CAP_BYTES` = `RUN_BYTES`
/// * `membership_bitmap_bytes(span)` = `span.div_ceil(8)`
/// * bitmap viable iff `span.div_ceil(8) <= 8_388_608` iff `span <= 67_108_864`
/// * `Overflow` is returned only when `!bitmap_viable` and the Vec reaches
///   `MEMBERSHIP_SET_CAP` = `RUN_BYTES / size_of::<u64>()` = 1_048_576
///
/// Smallest collection span that forces Overflow: **67_108_865** sequences
/// (one past the bitmap-viable ceiling) AND a predicate that admits
/// 1_048_577 members. `collection_span` is one past the highest sequence
/// ever allocated, so that means at least 67_108_864 `put`s. That is more
/// than 5 million rows, so this test is `#[ignore]` rather than constructed.
///
/// The named refusal, when it does fire, is
/// `BudgetExceeded { resource: ScalarPostings | SpatialPostings, limit: 1_048_576, attempted: 1_048_577 }`
/// (`NodeGate::build`, `src/query/membership.rs`).
#[ignore]
#[test]
fn o_node_predicate_membership_overflow_needs_67_million_sequences() {
    const RUN_BYTES: u64 = 8 << 20;
    let min_span = RUN_BYTES * 8 + 1;
    let vec_cap = RUN_BYTES / 8;
    assert_eq!(min_span, 67_108_865);
    assert_eq!(vec_cap, 1_048_576);
    assert!(
        min_span > 5_000_000,
        "the brief says ignore above 5M rows; span {min_span} is above that"
    );
}
