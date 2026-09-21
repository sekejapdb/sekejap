//! Adversarial oracle testing of query-engine surface combinations.
//! Failures are deliverables: keep each distinct failing case as a named test.
use sekejap_core::{
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, EntityId, Error, GeometryFilter,
        IndexId, PointFilter, Projection, QueryBudget, QueryDriver, QueryError, QueryFilter,
        QueryOrder, QueryPage, QueryRequest, QueryRow, QueryWork, ScalarFilter, ScalarValue,
        ScoreExpr, SortDirection, TextCandidates, TextMatch, VectorMetric, WorkResource,
    },
    spatial_geometry,
    spatial_math::{wgs84_distance_metres, within_radius, Bounds, Point},
    Kind,
};
use kernel::{
    io::IoMode,
    spatial::Geom,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ops::Bound,
    path::{Path, PathBuf},
};

const N_ROWS: usize = 2_000;
const N_EXTRA: usize = 200;
const SEED: u64 = 0x5345_4B45_4A41_5021;
const DIM: usize = 8;
const VOCAB: [&str; 30] = [
    "amber", "birch", "cedar", "delta", "ember", "flood", "grain", "harbour", "inlet", "jasper",
    "kestrel", "lagoon", "maple", "north", "osprey", "pine", "quay", "river", "stone", "tide",
    "umber", "valley", "willow", "xenon", "yellow", "zephyr", "anchor", "bridge", "canyon", "dune",
];
const KINDS: [&str; 6] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
const CLUSTERS: [(f64, f64); 5] = [
    (144.96, -37.81),
    (145.10, -37.90),
    (144.80, -37.70),
    (145.20, -37.75),
    (144.70, -37.95),
];
const PHRASE: &str = "amber willow";
const ANY_TERM: &str = "flood";
const ALL_TERMS: &str = "river harbour";
const PROJECTION: [&str; 5] = ["name", "born", "loc", "emb", "missing_field"];

fn cfg() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn generous() -> QueryBudget {
    QueryBudget::unlimited()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn usize(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() as usize) % n
        }
    }
    fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 / ((1u64 << 53) as f64)
    }
    fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
}

#[derive(Clone, Copy)]
struct Indexes {
    body: IndexId,
    born: IndexId,
    kind: IndexId,
    flag: IndexId,
    score: IndexId,
    loc: IndexId,
    plot: IndexId,
    emb_exact: IndexId,
    emb_quant: IndexId,
}

#[derive(Clone)]
struct LiveRow {
    id: EntityId,
    key: String,
    name: String,
    body: Option<String>,
    born: i64,
    score: Option<f64>,
    flag: bool,
    /// None = JSON null (present null). Some = text. Kind is never missing.
    kind: Option<String>,
    loc: Option<Point>,
    plot: Option<Geom>,
    emb: Option<[f32; DIM]>,
    tag: Option<String>,
}

#[derive(Clone, Debug)]
enum OwnedFilter {
    EqKind(&'static str),
    RangeBorn { lo: i64, hi: i64 },
    EqFlag(bool),
    IsNullKind,
    IsMissingScore,
    PointRadius { lon: f64, lat: f64, metres: f64 },
    PointBbox { west: f64, east: f64, south: f64, north: f64 },
    GeomIntersects,
    GeomWithin,
    GeomContains,
    GeomDWithin { metres: f64 },
    TextAny(&'static str),
    TextAll(&'static str),
    TextPhrase(&'static str),
    KeyRange { lo: &'static str, hi: &'static str },
}

#[derive(Clone, Copy, Debug)]
enum ScoreKind {
    DivByScore,
    DistanceMissing,
    HybridBm25Vec,
    BornMinusDist,
}

#[derive(Clone, Debug)]
enum OwnedOrder {
    EntityId,
    BornAsc,
    BornDesc,
    KindAsc,
    Driver,
    Distance,
    Bm25,
    ExactVector(VectorMetric),
    ApproxVector(VectorMetric),
    Score(ScoreKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedDriver {
    Auto,
    Entities,
    Filter(usize),
    Order,
    Keys,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedProj {
    Ids,
    Fields,
}

#[derive(Clone)]
struct Combo {
    name: String,
    why: &'static str,
    filters: Vec<OwnedFilter>,
    order: OwnedOrder,
    driver: OwnedDriver,
    proj: OwnedProj,
    page: usize,
    limit: Option<usize>,
}

struct Meta {
    path: PathBuf,
    collection: CollectionId,
    idx: Indexes,
    live: Vec<LiveRow>,
    q_point: Point,
    q_intersects: Geom,
    q_within: Geom,
    q_contains: Geom,
    q_dwithin: Geom,
    q_vec: [f32; DIM],
}

struct Failure {
    name: String,
    combo: String,
    kind: String,
    expected: String,
    actual: String,
    hypothesis: String,
}

fn unit8(mut v: [f32; DIM]) -> [f32; DIM] {
    let mut n = 0.0f64;
    for x in &v {
        n += f64::from(*x) * f64::from(*x);
    }
    if n == 0.0 {
        v[0] = 1.0;
        return v;
    }
    let s = n.sqrt() as f32;
    for x in &mut v {
        *x /= s;
    }
    v
}

fn cluster_center(i: usize) -> [f32; DIM] {
    let mut v = [0.0f32; DIM];
    match i % 5 {
        0 => v[0] = 1.0,
        1 => v[1] = 1.0,
        2 => v[2] = 1.0,
        3 => v[3] = 1.0,
        _ => {
            v[4] = 0.6;
            v[5] = 0.8;
        }
    }
    unit8(v)
}

fn vector_distance(stored: &[f32; DIM], query: &[f32], metric: VectorMetric) -> Option<f64> {
    let mut dot = 0.0;
    let mut sn = 0.0;
    let mut l2 = 0.0;
    let mut qn = 0.0;
    for i in 0..DIM {
        let s = f64::from(stored[i]);
        let q = f64::from(query[i]);
        dot += s * q;
        sn += s * s;
        qn += q * q;
        let d = s - q;
        l2 += d * d;
    }
    let mut distance = match metric {
        VectorMetric::SquaredL2 => l2,
        VectorMetric::NegativeDot => -dot,
        VectorMetric::Cosine if sn == 0.0 => return None,
        VectorMetric::Cosine => 1.0 - dot / (sn.sqrt() * qn.sqrt()),
    };
    if distance == 0.0 {
        distance = 0.0;
    }
    Some(distance)
}

fn signed_area(ring: &[[f64; 2]]) -> f64 {
    let mut a = 0.0;
    for w in ring.windows(2) {
        a += w[0][0] * w[1][1] - w[1][0] * w[0][1];
    }
    a * 0.5
}

fn close_ring(mut ring: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    if ring.first() != ring.last() {
        if let Some(first) = ring.first().copied() {
            ring.push(first);
        }
    }
    ring
}

fn ensure_winding(ring: &mut [[f64; 2]], ccw: bool) {
    let pos = signed_area(ring) > 0.0;
    if pos != ccw {
        ring.reverse();
    }
}

fn polygon_around(lon: f64, lat: f64, metres: f64, verts: usize, hole: bool, rng: &mut Rng) -> Geom {
    let dlat = metres / 111_320.0;
    let cos = lat.to_radians().cos().abs().max(0.2);
    let dlon = metres / (111_320.0 * cos);
    let n = verts.max(3);
    let mut ring = Vec::with_capacity(n + 1);
    let start = rng.f64() * std::f64::consts::TAU;
    for k in 0..n {
        let a = start + (k as f64) * std::f64::consts::TAU / (n as f64);
        let j = 0.85 + 0.15 * rng.f64();
        ring.push([lon + dlon * j * a.cos(), lat + dlat * j * a.sin()]);
    }
    ring = close_ring(ring);
    ensure_winding(&mut ring, true);
    let mut rings = vec![ring];
    if hole {
        let mut inner = Vec::with_capacity(n + 1);
        for k in 0..n {
            let a = start + (k as f64) * std::f64::consts::TAU / (n as f64);
            inner.push([lon + dlon * 0.25 * a.cos(), lat + dlat * 0.25 * a.sin()]);
        }
        inner = close_ring(inner);
        ensure_winding(&mut inner, false);
        rings.push(inner);
    }
    Geom::Polygon(rings)
}

fn geom_to_json(geom: &Geom) -> Value {
    match geom {
        Geom::Point(x, y) => json!({"type": "Point", "coordinates": [x, y]}),
        Geom::LineString(c) => json!({"type": "LineString", "coordinates": c}),
        Geom::Polygon(rings) => json!({"type": "Polygon", "coordinates": rings}),
        Geom::MultiPoint(c) => json!({"type": "MultiPoint", "coordinates": c}),
        Geom::MultiLineString(rs) => json!({"type": "MultiLineString", "coordinates": rs}),
        Geom::MultiPolygon(ps) => json!({"type": "MultiPolygon", "coordinates": ps}),
    }
}

fn point_json(p: Point) -> Value {
    json!({"type": "Point", "coordinates": [p.longitude(), p.latitude()]})
}

fn two_words(rng: &mut Rng) -> String {
    format!(
        "{} {}",
        VOCAB[rng.usize(VOCAB.len())],
        VOCAB[rng.usize(VOCAB.len())]
    )
}

fn body_text(rng: &mut Rng, plant_phrase: bool) -> String {
    let n = 5 + rng.usize(11);
    let mut words: Vec<&str> = (0..n).map(|_| VOCAB[rng.usize(VOCAB.len())]).collect();
    if plant_phrase && n >= 2 {
        let at = rng.usize(n - 1);
        words[at] = "amber";
        words[at + 1] = "willow";
    }
    words.join(" ")
}

fn row_to_json(row: &LiveRow) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("name".into(), json!(row.name));
    if let Some(body) = &row.body {
        obj.insert("body".into(), json!(body));
    }
    obj.insert("born".into(), json!(row.born));
    if let Some(score) = row.score {
        obj.insert("score".into(), json!(score));
    }
    obj.insert("flag".into(), json!(row.flag));
    match &row.kind {
        Some(k) => {
            obj.insert("kind".into(), json!(k));
        }
        None => {
            obj.insert("kind".into(), Value::Null);
        }
    }
    if let Some(loc) = row.loc {
        obj.insert("loc".into(), point_json(loc));
    }
    if let Some(plot) = &row.plot {
        obj.insert("plot".into(), geom_to_json(plot));
    }
    if let Some(emb) = row.emb {
        obj.insert("emb".into(), json!(emb.to_vec()));
    }
    if let Some(tag) = &row.tag {
        obj.insert("tag".into(), json!(tag));
    }
    Value::Object(obj)
}

fn generate_row(i: usize, rng: &mut Rng, with_tag: bool) -> (String, LiveRow) {
    let key = format!("k{i:04}");
    let cluster = i % 5;
    let (clon, clat) = CLUSTERS[cluster];
    let loc = if rng.chance(0.10) {
        None
    } else {
        let lon = (clon + (rng.f64() - 0.5) * 0.04).clamp(-180.0, 180.0);
        let lat = (clat + (rng.f64() - 0.5) * 0.03).clamp(-90.0, 90.0);
        Some(Point::new(lon, lat).expect("point"))
    };
    let plot = if rng.chance(0.10) {
        None
    } else {
        let (lon, lat) = match loc {
            Some(p) => (p.longitude(), p.latitude()),
            None => (
                clon + (rng.f64() - 0.5) * 0.04,
                clat + (rng.f64() - 0.5) * 0.03,
            ),
        };
        let metres = 50.0 + rng.f64() * 450.0;
        let verts = if rng.chance(0.5) { 3 } else { 4 };
        Some(polygon_around(lon, lat, metres, verts, rng.chance(0.10), rng))
    };
    let emb = if rng.chance(0.10) {
        None
    } else {
        let mut v = cluster_center(cluster);
        for x in &mut v {
            *x += (rng.f64() as f32 - 0.5) * 0.08;
        }
        Some(unit8(v))
    };
    let score = if rng.chance(0.08) {
        None
    } else if rng.chance(0.04) {
        Some(0.0)
    } else {
        Some((rng.f64() * 100.0).round() / 4.0)
    };
    let row = LiveRow {
        id: EntityId {
            collection: CollectionId(0),
            sequence: 0,
        },
        key: key.clone(),
        name: two_words(rng),
        body: if rng.chance(0.05) {
            None
        } else {
            let plant = rng.chance(0.08);
            Some(body_text(rng, plant))
        },
        born: 1940 + (i as i64 % 80),
        score,
        flag: rng.chance(0.45),
        kind: if rng.chance(0.05) {
            None
        } else {
            Some(KINDS[i % 6].to_string())
        },
        loc,
        plot,
        emb,
        tag: if with_tag && rng.chance(0.7) {
            Some(VOCAB[rng.usize(VOCAB.len())].to_string())
        } else {
            None
        },
    };
    (key, row)
}

fn build_index(db: &mut Database, id: IndexId) {
    db.build_index_to_ready(id, 256).unwrap();
}

fn build_fixture(path: &Path) -> Meta {
    let mut rng = Rng(SEED);
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "people",
            vec![
                ("name".into(), Kind::Text),
                ("body".into(), Kind::Text),
                ("born".into(), Kind::Int),
                ("score".into(), Kind::Real),
                ("flag".into(), Kind::Bool),
                ("kind".into(), Kind::Text),
                ("loc".into(), Kind::Point),
                ("plot".into(), Kind::Geo),
                ("emb".into(), Kind::Vector(DIM)),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut live = Vec::with_capacity(N_ROWS + N_EXTRA);
    for i in 0..N_ROWS {
        let (key, mut row) = generate_row(i, &mut rng, false);
        let id = db.put(collection, &key, &row_to_json(&row)).unwrap();
        row.id = id;
        live.push(row);
    }
    db.commit().unwrap();
    let idx = Indexes {
        body: db.create_text_index(collection, "body", "body").unwrap(),
        born: db
            .create_scalar_index(collection, "born", "born", false)
            .unwrap(),
        kind: db
            .create_scalar_index(collection, "kind", "kind", false)
            .unwrap(),
        flag: db
            .create_scalar_index(collection, "flag", "flag", false)
            .unwrap(),
        score: db
            .create_scalar_index(collection, "score", "score", false)
            .unwrap(),
        loc: db.create_point_index(collection, "loc", "loc").unwrap(),
        plot: db.create_geometry_index(collection, "plot", "plot").unwrap(),
        emb_exact: db
            .create_exact_vector_index(collection, "emb", "emb")
            .unwrap(),
        emb_quant: db
            .create_quantized_vector_index(collection, "emb_int8", "emb")
            .unwrap(),
    };
    db.commit().unwrap();
    for index in [
        idx.body,
        idx.born,
        idx.kind,
        idx.flag,
        idx.score,
        idx.loc,
        idx.plot,
        idx.emb_exact,
        idx.emb_quant,
    ] {
        build_index(&mut db, index);
    }
    db.commit().unwrap();

    let n = live.len();
    let n_del = ((n as f64) * 0.03).round() as usize;
    let n_upd = ((n as f64) * 0.03).round() as usize;
    let mut del_idx: Vec<usize> = (0..n).collect();
    for i in 0..n {
        let j = rng.usize(n);
        del_idx.swap(i, j);
    }
    let delete_at: Vec<usize> = del_idx[..n_del].to_vec();
    let mut delete_set: HashSet<usize> = delete_at.iter().copied().collect();
    for i in delete_at {
        db.delete(collection, &live[i].key).unwrap();
    }
    live = live
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !delete_set.remove(i))
        .map(|(_, r)| r)
        .collect();

    let mut upd: Vec<usize> = (0..live.len()).collect();
    for i in 0..upd.len() {
        let j = rng.usize(upd.len());
        upd.swap(i, j);
    }
    for &i in upd.iter().take(n_upd) {
        let cluster = rng.usize(5);
        let (clon, clat) = CLUSTERS[cluster];
        let lon = clon + (rng.f64() - 0.5) * 0.04;
        let lat = clat + (rng.f64() - 0.5) * 0.03;
        live[i].loc = Some(Point::new(lon, lat).unwrap());
        let mut v = cluster_center(cluster);
        for x in &mut v {
            *x += (rng.f64() as f32 - 0.5) * 0.05;
        }
        live[i].emb = Some(unit8(v));
        live[i].kind = Some(KINDS[rng.usize(6)].to_string());
        db.update(
            collection,
            &live[i].key,
            &json!({
                "loc": point_json(live[i].loc.unwrap()),
                "emb": live[i].emb.unwrap().to_vec(),
                "kind": live[i].kind.clone().unwrap(),
            }),
        )
        .unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();

    let mut fields = db.collection_info(collection).unwrap().layout.fields;
    let pos = fields.iter().position(|(n, _)| n == "emb").expect("emb");
    fields.insert(pos, ("tag".into(), Kind::Text));
    db.alter_collection(collection, fields).unwrap();
    db.commit().unwrap();

    for i in N_ROWS..N_ROWS + N_EXTRA {
        let (key, mut row) = generate_row(i, &mut rng, true);
        let id = db.put(collection, &key, &row_to_json(&row)).unwrap();
        row.id = id;
        live.push(row);
    }
    db.commit().unwrap();
    drop(db);

    let q_point = Point::new(CLUSTERS[0].0, CLUSTERS[0].1).unwrap();
    let mut rng2 = Rng(SEED ^ 0xD00D);
    let q_intersects = polygon_around(CLUSTERS[0].0, CLUSTERS[0].1, 2_500.0, 4, false, &mut rng2);
    let q_within = polygon_around(CLUSTERS[0].0, CLUSTERS[0].1, 8_000.0, 4, false, &mut rng2);
    let q_contains = polygon_around(CLUSTERS[0].0, CLUSTERS[0].1, 8.0, 3, false, &mut rng2);
    let q_dwithin = Geom::Point(CLUSTERS[0].0, CLUSTERS[0].1);
    let q_vec = cluster_center(0);

    Meta {
        path: path.to_path_buf(),
        collection,
        idx,
        live,
        q_point,
        q_intersects,
        q_within,
        q_contains,
        q_dwithin,
        q_vec,
    }
}

impl OwnedFilter {
    fn to_query<'a>(&'a self, meta: &'a Meta) -> QueryFilter<'a> {
        match self {
            OwnedFilter::EqKind(k) => QueryFilter::Scalar {
                index: meta.idx.kind,
                predicate: ScalarFilter::Eq(ScalarValue::Text(k)),
            },
            OwnedFilter::RangeBorn { lo, hi } => QueryFilter::Scalar {
                index: meta.idx.born,
                predicate: ScalarFilter::Range {
                    lower: Bound::Included(ScalarValue::I64(*lo)),
                    upper: Bound::Included(ScalarValue::I64(*hi)),
                },
            },
            OwnedFilter::EqFlag(v) => QueryFilter::Scalar {
                index: meta.idx.flag,
                predicate: ScalarFilter::Eq(ScalarValue::Bool(*v)),
            },
            OwnedFilter::IsNullKind => QueryFilter::Scalar {
                index: meta.idx.kind,
                predicate: ScalarFilter::IsNull,
            },
            OwnedFilter::IsMissingScore => QueryFilter::Scalar {
                index: meta.idx.score,
                predicate: ScalarFilter::IsMissing,
            },
            OwnedFilter::PointRadius { lon, lat, metres } => QueryFilter::Point {
                index: meta.idx.loc,
                predicate: PointFilter::Radius {
                    center: Point::new(*lon, *lat).unwrap(),
                    radius_metres: *metres,
                },
            },
            OwnedFilter::PointBbox {
                west,
                east,
                south,
                north,
            } => QueryFilter::Point {
                index: meta.idx.loc,
                predicate: PointFilter::Bbox(Bounds::new(*west, *east, *south, *north).unwrap()),
            },
            OwnedFilter::GeomIntersects => QueryFilter::Geometry {
                index: meta.idx.plot,
                predicate: GeometryFilter::Intersects(meta.q_intersects.clone()),
            },
            OwnedFilter::GeomWithin => QueryFilter::Geometry {
                index: meta.idx.plot,
                predicate: GeometryFilter::Within(meta.q_within.clone()),
            },
            OwnedFilter::GeomContains => QueryFilter::Geometry {
                index: meta.idx.plot,
                predicate: GeometryFilter::Contains(meta.q_contains.clone()),
            },
            OwnedFilter::GeomDWithin { metres } => QueryFilter::Geometry {
                index: meta.idx.plot,
                predicate: GeometryFilter::DWithin {
                    geometry: meta.q_dwithin.clone(),
                    metres: *metres,
                },
            },
            OwnedFilter::TextAny(q) => QueryFilter::Text {
                index: meta.idx.body,
                query: q,
                matching: TextMatch::Any,
            },
            OwnedFilter::TextAll(q) => QueryFilter::Text {
                index: meta.idx.body,
                query: q,
                matching: TextMatch::All,
            },
            OwnedFilter::TextPhrase(q) => QueryFilter::Text {
                index: meta.idx.body,
                query: q,
                matching: TextMatch::Phrase,
            },
            OwnedFilter::KeyRange { lo, hi } => QueryFilter::Key {
                lower: Bound::Included(*lo),
                upper: Bound::Excluded(*hi),
            },
        }
    }

    fn is_key(&self) -> bool {
        matches!(self, OwnedFilter::KeyRange { .. })
    }
    fn is_geometry(&self) -> bool {
        matches!(
            self,
            OwnedFilter::GeomIntersects
                | OwnedFilter::GeomWithin
                | OwnedFilter::GeomContains
                | OwnedFilter::GeomDWithin { .. }
        )
    }
    fn is_phrase(&self) -> bool {
        matches!(self, OwnedFilter::TextPhrase(_))
    }
    fn is_drivable(&self) -> bool {
        !self.is_key()
    }
    fn label(&self) -> String {
        match self {
            OwnedFilter::EqKind(k) => format!("eq_kind={k}"),
            OwnedFilter::RangeBorn { lo, hi } => format!("born[{lo}..{hi}]"),
            OwnedFilter::EqFlag(v) => format!("flag={v}"),
            OwnedFilter::IsNullKind => "kind_isnull".into(),
            OwnedFilter::IsMissingScore => "score_ismissing".into(),
            OwnedFilter::PointRadius { metres, .. } => format!("radius_{metres}m"),
            OwnedFilter::PointBbox { .. } => "bbox".into(),
            OwnedFilter::GeomIntersects => "g_intersects".into(),
            OwnedFilter::GeomWithin => "g_within".into(),
            OwnedFilter::GeomContains => "g_contains".into(),
            OwnedFilter::GeomDWithin { metres } => format!("g_dwithin_{metres}"),
            OwnedFilter::TextAny(q) => format!("text_any:{q}"),
            OwnedFilter::TextAll(q) => format!("text_all:{q}"),
            OwnedFilter::TextPhrase(q) => format!("text_phrase:{q}"),
            OwnedFilter::KeyRange { lo, hi } => format!("key[{lo}..{hi})"),
        }
    }
}

impl OwnedOrder {
    fn label(&self) -> &'static str {
        match self {
            OwnedOrder::EntityId => "id",
            OwnedOrder::BornAsc => "born_asc",
            OwnedOrder::BornDesc => "born_desc",
            OwnedOrder::KindAsc => "kind_asc",
            OwnedOrder::Driver => "driver",
            OwnedOrder::Distance => "distance",
            OwnedOrder::Bm25 => "bm25",
            OwnedOrder::ExactVector(VectorMetric::Cosine) => "vec_cos",
            OwnedOrder::ExactVector(VectorMetric::SquaredL2) => "vec_l2",
            OwnedOrder::ExactVector(VectorMetric::NegativeDot) => "vec_ndot",
            OwnedOrder::ApproxVector(VectorMetric::Cosine) => "ann_cos",
            OwnedOrder::ApproxVector(VectorMetric::SquaredL2) => "ann_l2",
            OwnedOrder::ApproxVector(VectorMetric::NegativeDot) => "ann_ndot",
            OwnedOrder::Score(ScoreKind::DivByScore) => "score_div",
            OwnedOrder::Score(ScoreKind::DistanceMissing) => "score_dist",
            OwnedOrder::Score(ScoreKind::HybridBm25Vec) => "score_hybrid",
            OwnedOrder::Score(ScoreKind::BornMinusDist) => "score_born_dist",
        }
    }
    fn is_total(&self) -> bool {
        !matches!(self, OwnedOrder::Driver)
    }
    fn is_score(&self) -> bool {
        matches!(self, OwnedOrder::Score(_))
    }
}

impl Combo {
    fn label(&self) -> String {
        let fs: Vec<String> = self.filters.iter().map(OwnedFilter::label).collect();
        format!(
            "{} | filters=[{}] order={} driver={:?} proj={:?} page={} limit={:?}",
            self.name,
            fs.join(","),
            self.order.label(),
            self.driver,
            self.proj,
            self.page,
            self.limit
        )
    }
    fn expects_prepare_refuse(&self) -> bool {
        // A key range is the row's own first field, so under any driver but
        // Keys it is answered from the row (QL_CONTRACT §6 names the cost);
        // it is no longer a prepare refusal. Only an undrivable Filter(i) is.
        if let OwnedDriver::Filter(i) = self.driver {
            if i >= self.filters.len() || !self.filters[i].is_drivable() {
                return true;
            }
        }
        false
    }
}

fn nan_last_cmp(a: f64, b: f64, desc: bool) -> std::cmp::Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => {
            if desc {
                b.total_cmp(&a)
            } else {
                a.total_cmp(&b)
            }
        }
    }
}

struct Oracle<'a> {
    meta: &'a Meta,
    db: &'a Database,
    bm25: std::cell::RefCell<HashMap<(String, u8), BTreeMap<EntityId, f64>>>,
}

impl<'a> Oracle<'a> {
    fn new(meta: &'a Meta, db: &'a Database) -> Self {
        Self {
            meta,
            db,
            bm25: std::cell::RefCell::new(HashMap::new()),
        }
    }

    fn bm25_map(&self, query: &str, matching: TextMatch) -> BTreeMap<EntityId, f64> {
        let tag = match matching {
            TextMatch::Any => 0u8,
            TextMatch::All => 1,
            TextMatch::Phrase => 2,
        };
        let key = (query.to_string(), tag);
        if let Some(found) = self.bm25.borrow().get(&key) {
            return found.clone();
        }
        let hits = self
            .db
            .query_text(
                self.meta.idx.body,
                query,
                matching,
                65_536,
                TextCandidates::All,
                10_000_000,
                || false,
            )
            .unwrap_or_default();
        let map: BTreeMap<EntityId, f64> = hits.into_iter().map(|h| (h.id, h.score)).collect();
        self.bm25.borrow_mut().insert(key, map.clone());
        map
    }

    fn eval_score(&self, row: &LiveRow, kind: ScoreKind) -> f64 {
        match kind {
            ScoreKind::DivByScore => {
                let denom = row.score.unwrap_or(0.0);
                if denom == 0.0 {
                    f64::NAN
                } else {
                    1.0 / denom
                }
            }
            ScoreKind::DistanceMissing => match row.loc {
                Some(p) => wgs84_distance_metres(self.meta.q_point, p),
                None => f64::INFINITY,
            },
            ScoreKind::HybridBm25Vec => {
                let bm = row
                    .body
                    .as_ref()
                    .and_then(|_| self.bm25_map(ANY_TERM, TextMatch::Any).get(&row.id).copied())
                    .unwrap_or(0.0);
                let sim = match row.emb {
                    Some(e) => vector_distance(&e, &self.meta.q_vec, VectorMetric::Cosine)
                        .map(|d| -d)
                        .unwrap_or(f64::NEG_INFINITY),
                    None => f64::NEG_INFINITY,
                };
                bm + sim
            }
            ScoreKind::BornMinusDist => {
                let dist = match row.loc {
                    Some(p) => wgs84_distance_metres(self.meta.q_point, p),
                    None => f64::INFINITY,
                };
                (row.born as f64) - dist
            }
        }
    }

    fn filter_matches(&self, row: &LiveRow, f: &OwnedFilter) -> bool {
        match f {
            OwnedFilter::EqKind(k) => row.kind.as_deref() == Some(*k),
            OwnedFilter::RangeBorn { lo, hi } => row.born >= *lo && row.born <= *hi,
            OwnedFilter::EqFlag(v) => row.flag == *v,
            OwnedFilter::IsNullKind => row.kind.is_none(),
            OwnedFilter::IsMissingScore => row.score.is_none(),
            OwnedFilter::PointRadius { lon, lat, metres } => match row.loc {
                Some(p) => {
                    let c = Point::new(*lon, *lat).unwrap();
                    within_radius(c, p, *metres).unwrap()
                }
                None => false,
            },
            OwnedFilter::PointBbox {
                west,
                east,
                south,
                north,
            } => match row.loc {
                Some(p) => Bounds::new(*west, *east, *south, *north).unwrap().contains(p),
                None => false,
            },
            OwnedFilter::GeomIntersects => row
                .plot
                .as_ref()
                .is_some_and(|g| spatial_geometry::intersects(g, &self.meta.q_intersects)),
            OwnedFilter::GeomWithin => row
                .plot
                .as_ref()
                .is_some_and(|g| spatial_geometry::within(g, &self.meta.q_within)),
            OwnedFilter::GeomContains => row
                .plot
                .as_ref()
                .is_some_and(|g| spatial_geometry::contains(g, &self.meta.q_contains)),
            OwnedFilter::GeomDWithin { metres } => row
                .plot
                .as_ref()
                .is_some_and(|g| spatial_geometry::dwithin_m(g, &self.meta.q_dwithin, *metres)),
            OwnedFilter::TextAny(q) => self.bm25_map(q, TextMatch::Any).contains_key(&row.id),
            OwnedFilter::TextAll(q) => self.bm25_map(q, TextMatch::All).contains_key(&row.id),
            OwnedFilter::TextPhrase(q) => self.bm25_map(q, TextMatch::Phrase).contains_key(&row.id),
            OwnedFilter::KeyRange { lo, hi } => {
                row.key.as_str() >= *lo && row.key.as_str() < *hi
            }
        }
    }

    fn candidates(&self, filters: &[OwnedFilter]) -> Vec<&LiveRow> {
        self.meta
            .live
            .iter()
            .filter(|row| filters.iter().all(|f| self.filter_matches(row, f)))
            .collect()
    }

    /// Ranked (rank_value_debug, id) in engine order. Driver order is unsorted.
    fn oracle(&self, combo: &Combo) -> Vec<EntityId> {
        let mut rows = self.candidates(&combo.filters);
        match &combo.order {
            OwnedOrder::EntityId => rows.sort_by_key(|r| r.id),
            OwnedOrder::BornAsc => rows.sort_by(|a, b| a.born.cmp(&b.born).then(a.id.cmp(&b.id))),
            OwnedOrder::BornDesc => rows.sort_by(|a, b| b.born.cmp(&a.born).then(a.id.cmp(&b.id))),
            OwnedOrder::KindAsc => rows.sort_by(|a, b| match (&a.kind, &b.kind) {
                (None, None) => a.id.cmp(&b.id),
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(x), Some(y)) => x.cmp(y).then(a.id.cmp(&b.id)),
            }),
            OwnedOrder::Driver => {}
            OwnedOrder::Distance => {
                rows.retain(|r| r.loc.is_some());
                rows.sort_by(|a, b| {
                    let da = wgs84_distance_metres(self.meta.q_point, a.loc.unwrap());
                    let db = wgs84_distance_metres(self.meta.q_point, b.loc.unwrap());
                    da.total_cmp(&db).then(a.id.cmp(&b.id))
                });
            }
            OwnedOrder::Bm25 => {
                let map = self.bm25_map(ANY_TERM, TextMatch::Any);
                rows.retain(|r| map.contains_key(&r.id));
                rows.sort_by(|a, b| {
                    let sa = map[&a.id];
                    let sb = map[&b.id];
                    nan_last_cmp(sa, sb, true).then(a.id.cmp(&b.id))
                });
            }
            OwnedOrder::ExactVector(metric) | OwnedOrder::ApproxVector(metric) => {
                let m = *metric;
                rows.retain(|r| r.emb.is_some() && vector_distance(&r.emb.unwrap(), &self.meta.q_vec, m).is_some());
                rows.sort_by(|a, b| {
                    let da = vector_distance(&a.emb.unwrap(), &self.meta.q_vec, m).unwrap();
                    let db = vector_distance(&b.emb.unwrap(), &self.meta.q_vec, m).unwrap();
                    da.total_cmp(&db).then(a.id.cmp(&b.id))
                });
            }
            OwnedOrder::Score(kind) => {
                let k = *kind;
                let desc = matches!(k, ScoreKind::HybridBm25Vec);
                rows.sort_by(|a, b| {
                    let sa = self.eval_score(a, k);
                    let sb = self.eval_score(b, k);
                    nan_last_cmp(sa, sb, desc).then(a.id.cmp(&b.id))
                });
            }
        }
        if let Some(limit) = combo.limit {
            rows.truncate(limit);
        }
        rows.into_iter().map(|r| r.id).collect()
    }
}

fn with_prepared<T>(
    db: &Database,
    meta: &Meta,
    combo: &Combo,
    f: impl FnOnce(Result<sekejap_core::collections::PreparedQuery<'_>, QueryError>) -> T,
) -> T {
    let filters: Vec<QueryFilter> = combo.filters.iter().map(|x| x.to_query(meta)).collect();
    let qvec = meta.q_vec;
    let lit1 = ScoreExpr::Lit(1.0);
    let scalar_score = ScoreExpr::Scalar {
        index: meta.idx.score,
    };
    let scalar_born = ScoreExpr::Scalar {
        index: meta.idx.born,
    };
    let dist = ScoreExpr::Distance {
        index: meta.idx.loc,
        center: meta.q_point,
    };
    let bm25 = ScoreExpr::Bm25 {
        index: meta.idx.body,
        query: ANY_TERM,
        matching: TextMatch::Any,
    };
    let vsim = ScoreExpr::VectorSimilarity {
        index: meta.idx.emb_exact,
        query: &qvec,
        metric: VectorMetric::Cosine,
    };
    let div = ScoreExpr::Div(&lit1, &scalar_score);
    let hybrid = ScoreExpr::Add(&bm25, &vsim);
    let born_minus = ScoreExpr::Sub(&scalar_born, &dist);
    let order = match &combo.order {
        OwnedOrder::EntityId => QueryOrder::EntityId,
        OwnedOrder::BornAsc => QueryOrder::Scalar {
            index: meta.idx.born,
            direction: SortDirection::Ascending,
        },
        OwnedOrder::BornDesc => QueryOrder::Scalar {
            index: meta.idx.born,
            direction: SortDirection::Descending,
        },
        OwnedOrder::KindAsc => QueryOrder::Scalar {
            index: meta.idx.kind,
            direction: SortDirection::Ascending,
        },
        OwnedOrder::Driver => QueryOrder::Driver,
        OwnedOrder::Distance => QueryOrder::Distance {
            index: meta.idx.loc,
            center: meta.q_point,
            direction: SortDirection::Ascending,
        },
        OwnedOrder::Bm25 => QueryOrder::Bm25 {
            index: meta.idx.body,
            query: ANY_TERM,
            matching: TextMatch::Any,
        },
        OwnedOrder::ExactVector(m) => QueryOrder::ExactVector {
            index: meta.idx.emb_exact,
            query: &qvec,
            metric: *m,
        },
        OwnedOrder::ApproxVector(m) => QueryOrder::ApproximateVector {
            index: meta.idx.emb_quant,
            query: &qvec,
            metric: *m,
            ef: 4096,
        },
        OwnedOrder::Score(ScoreKind::DivByScore) => QueryOrder::Score {
            expr: &div,
            direction: SortDirection::Ascending,
        },
        OwnedOrder::Score(ScoreKind::DistanceMissing) => QueryOrder::Score {
            expr: &dist,
            direction: SortDirection::Ascending,
        },
        OwnedOrder::Score(ScoreKind::HybridBm25Vec) => QueryOrder::Score {
            expr: &hybrid,
            direction: SortDirection::Descending,
        },
        OwnedOrder::Score(ScoreKind::BornMinusDist) => QueryOrder::Score {
            expr: &born_minus,
            direction: SortDirection::Ascending,
        },
    };
    let driver = match combo.driver {
        OwnedDriver::Auto => CandidateDriver::Auto,
        OwnedDriver::Entities => CandidateDriver::Entities,
        OwnedDriver::Filter(i) => CandidateDriver::Filter(i),
        OwnedDriver::Order => CandidateDriver::Order,
        OwnedDriver::Keys => CandidateDriver::Keys,
    };
    let request = QueryRequest {
        collection: meta.collection,
        filters: &filters,
        order,
        projection: match combo.proj {
            OwnedProj::Ids => Projection::Ids,
            OwnedProj::Fields => Projection::Fields(&PROJECTION),
        },
        total_limit: combo.limit,
        driver,
    };
    f(db.prepare_query(request))
}

fn ids_of(rows: &[QueryRow]) -> Vec<EntityId> {
    rows.iter().map(|r| r.id).collect()
}

fn seqs(ids: &[EntityId]) -> Vec<u64> {
    ids.iter().map(|id| id.sequence).collect()
}

fn first_diff(expected: &[EntityId], actual: &[EntityId]) -> String {
    let n = expected.len().max(actual.len());
    for i in 0..n {
        let e = expected.get(i).map(|id| id.sequence);
        let a = actual.get(i).map(|id| id.sequence);
        if e != a {
            return format!("at {i}: expected seq {e:?} actual seq {a:?} (exp_len={} act_len={})", expected.len(), actual.len());
        }
    }
    format!("lens {} vs {}", expected.len(), actual.len())
}

fn work_of(w: &QueryWork, r: WorkResource) -> u64 {
    match r {
        WorkResource::Candidates => w.candidates,
        WorkResource::PrimaryReads => w.primary_reads,
        WorkResource::ScalarPostings => w.scalar_postings,
        WorkResource::GraphEdges => w.graph_edges,
        WorkResource::GraphVisited => w.graph_visited,
        WorkResource::SpatialPostings => w.spatial_postings,
        WorkResource::TextPostings => w.text_postings,
        WorkResource::TextTokens => w.text_tokens,
        WorkResource::VectorLocators => w.vector_locators,
        WorkResource::VectorSidecars => w.vector_sidecars,
        WorkResource::VectorLanes => w.vector_lanes,
        WorkResource::KeyPostings => w.key_postings,
        // A bounded WRITE pass is `tests/write_where.rs`; no query in this
        // matrix writes a row, so this resource is never charged here.
        WorkResource::RowsWritten => w.rows_written,
        // Aggregates are `tests/query_aggregate.rs`; no query in this file
        // folds groups, so this resource is never charged here.
        WorkResource::Groups => w.groups,
        // The peak bytes a BOOLEAN filter's intermediate sets held at once.
        // No combo in this matrix writes one, so it is never charged here
        // either; `tests/query_boolean.rs` is where it is exercised.
        WorkResource::MembershipBytes => w.membership_bytes,
        WorkResource::OutputBytes => w.output_bytes,
        // Wall clock, not work: it has no `QueryWork` counter and is not in
        // `RESOURCES`, so this arm is never reached.
        WorkResource::Deadline => 0,
    }
}

fn set_budget(mut b: QueryBudget, r: WorkResource, n: u64) -> QueryBudget {
    match r {
        WorkResource::Candidates => b.candidates = n,
        WorkResource::PrimaryReads => b.primary_reads = n,
        WorkResource::ScalarPostings => b.scalar_postings = n,
        WorkResource::GraphEdges => b.graph_edges = n,
        WorkResource::GraphVisited => b.graph_visited = n,
        WorkResource::SpatialPostings => b.spatial_postings = n,
        WorkResource::TextPostings => b.text_postings = n,
        WorkResource::TextTokens => b.text_tokens = n,
        WorkResource::VectorLocators => b.vector_locators = n,
        WorkResource::VectorSidecars => b.vector_sidecars = n,
        WorkResource::VectorLanes => b.vector_lanes = n,
        WorkResource::KeyPostings => b.key_postings = n,
        WorkResource::RowsWritten => b.rows_written = n,
        WorkResource::Groups => b.groups = n,
        // `MembershipBytes` has no `QueryBudget` field on purpose: its
        // ceiling is the fixed `RUN_BYTES` memory promise, which a caller
        // cannot raise by asking. It is therefore not in `RESOURCES` and
        // this arm is never reached.
        WorkResource::MembershipBytes => {}
        // `Deadline` is an instant, not an amount; `QueryBudget::with_deadline`
        // is how it is set and no combo in this matrix asks for one.
        WorkResource::Deadline => {}
        WorkResource::OutputBytes => b.output_bytes = n,
    }
    b
}

const RESOURCES: [WorkResource; 13] = [
    WorkResource::Candidates,
    WorkResource::PrimaryReads,
    WorkResource::ScalarPostings,
    WorkResource::GraphEdges,
    WorkResource::GraphVisited,
    WorkResource::SpatialPostings,
    WorkResource::TextPostings,
    WorkResource::TextTokens,
    WorkResource::VectorLocators,
    WorkResource::VectorSidecars,
    WorkResource::VectorLanes,
    WorkResource::KeyPostings,
    WorkResource::OutputBytes,
];

/// Combinations where every filter and the order are index-side, so a
/// `Projection::Ids` page must not open the primary tree (`winner_needs_no_row`
/// in `src/query.rs:6731` plus no row-only ranking).
///
/// Expected zero `primary_reads` only for:
/// - Scalar Eq/Range as the chosen driver, order EntityId or Driver
/// - Text Any/All as the chosen driver, order EntityId, Driver, or Bm25
/// - Keys driver, order EntityId or Driver
/// - Spatial/Nearest driver, order EntityId, Driver, or Distance
/// - Bm25 order (the norm is the liveness proof)
///
/// Not expected: IsNull/IsMissing (still probe), geometry refine, phrase
/// adjacency, Score leaves, vector orders (winner existence probe), scalar
/// order on a different index, Keys/Text driving a Distance/Scalar rank.
fn expect_zero_primary(combo: &Combo, driver: QueryDriver) -> bool {
    if combo.proj != OwnedProj::Ids {
        return false;
    }
    if combo.filters.iter().any(|f| f.is_geometry() || f.is_phrase()) {
        return false;
    }
    if combo.order.is_score()
        || matches!(
            combo.order,
            OwnedOrder::ExactVector(_) | OwnedOrder::ApproxVector(_)
        )
    {
        return false;
    }
    // IsNull/IsMissing are posting-walked when they DRIVE, but they are not
    // in `winner_needs_no_row`'s Eq|Range arm (`query.rs:6857`), and as
    // non-driving filters they force a row read. Never claim zero for them.
    if combo
        .filters
        .iter()
        .any(|f| matches!(f, OwnedFilter::IsNullKind | OwnedFilter::IsMissingScore))
    {
        return false;
    }
    let eq_or_range = combo.filters.iter().any(|f| {
        matches!(
            f,
            OwnedFilter::EqKind(_) | OwnedFilter::RangeBorn { .. } | OwnedFilter::EqFlag(_)
        )
    });
    match driver {
        QueryDriver::Text(_) => {
            combo.filters.iter().all(|f| {
                matches!(
                    f,
                    OwnedFilter::TextAny(_) | OwnedFilter::TextAll(_)
                )
            }) && matches!(
                combo.order,
                OwnedOrder::EntityId | OwnedOrder::Driver | OwnedOrder::Bm25
            )
        }
        QueryDriver::Keys => {
            combo.filters.iter().all(OwnedFilter::is_key)
                && matches!(combo.order, OwnedOrder::EntityId | OwnedOrder::Driver)
        }
        QueryDriver::Scalar(_) => {
            eq_or_range
                && combo.filters.iter().all(|f| {
                    matches!(
                        f,
                        OwnedFilter::EqKind(_)
                            | OwnedFilter::RangeBorn { .. }
                            | OwnedFilter::EqFlag(_)
                    )
                })
                && matches!(combo.order, OwnedOrder::EntityId | OwnedOrder::Driver | OwnedOrder::Bm25)
        }
        QueryDriver::Spatial { .. } | QueryDriver::Nearest { .. } => {
            combo.filters.iter().all(|f| {
                matches!(f, OwnedFilter::PointRadius { .. } | OwnedFilter::PointBbox { .. })
            }) && matches!(
                combo.order,
                OwnedOrder::EntityId | OwnedOrder::Driver | OwnedOrder::Distance
            )
        }
        // No combination in this matrix writes a boolean filter, so the
        // membership driver is never the one Auto picks here.
        QueryDriver::Entities
        | QueryDriver::Geometry { .. }
        | QueryDriver::Membership { .. }
        | QueryDriver::Graph { .. } => false,
        QueryDriver::ExactVector(_) | QueryDriver::QuantizedVector(_) => {
            matches!(combo.order, OwnedOrder::Bm25)
        }
    }
}

struct Drain {
    rows: Vec<QueryRow>,
    pages: Vec<QueryPage>,
}

fn drain_query(
    db: &Database,
    meta: &Meta,
    combo: &Combo,
) -> Result<Drain, QueryError> {
    with_prepared(db, meta, combo, |prep| {
        let mut prepared = prep?;
        let mut rows = Vec::new();
        let mut pages = Vec::new();
        let mut seen = BTreeSet::new();
        for _ in 0..4_000 {
            let page = prepared.next_page(combo.page, generous(), || false)?;
            for row in &page.rows {
                if !seen.insert(row.id) {
                    return Err(QueryError::Database(Error::InvalidInput(format!(
                        "duplicate id seq {}",
                        row.id.sequence
                    ))));
                }
            }
            let done = page.done;
            rows.extend(page.rows.iter().cloned());
            pages.push(page);
            if done {
                break;
            }
        }
        Ok(Drain { rows, pages })
    })
}

fn fail(combo: &Combo, kind: &str, expected: &str, actual: &str, hypothesis: &str) -> Failure {
    Failure {
        name: combo.name.clone(),
        combo: combo.label(),
        kind: kind.into(),
        expected: expected.into(),
        actual: actual.into(),
        hypothesis: hypothesis.into(),
    }
}

fn run_one(
    db: &Database,
    meta: &Meta,
    oracle: &Oracle<'_>,
    combo: &Combo,
    depth: Depth,
) -> Result<(), Failure> {
    if combo.expects_prepare_refuse() {
        let refused = with_prepared(db, meta, combo, |prep| prep.is_err());
        if !refused {
            return Err(fail(
                combo,
                "prepare",
                "prepare refuse (key filter needs Keys driver / undrivable Filter)",
                "prepare succeeded",
                "src/query.rs:2615 key filter requires CandidateDriver::Keys",
            ));
        }
        return Ok(());
    }
    let expected = oracle.oracle(combo);
    let drain = match drain_query(db, meta, combo) {
        Ok(d) => d,
        Err(QueryError::Database(Error::InvalidInput(msg)))
            if msg.starts_with("duplicate id") =>
        {
            return Err(fail(
                combo,
                "dup",
                "no duplicate ids across pages",
                &msg,
                "src/query.rs:7287 finish_page continuation / after cursor",
            ));
        }
        Err(e) => {
            return Err(fail(
                combo,
                "exec",
                "query succeeds",
                &format!("{e:?}"),
                "src/query.rs prepare_query / next_page",
            ));
        }
    };
    let actual = ids_of(&drain.rows);
    if combo.order.is_total() {
        if actual != expected {
            return Err(fail(
                combo,
                "order",
                &format!("{:?}", seqs(&expected)),
                &format!("{:?} ; {}", seqs(&actual), first_diff(&expected, &actual)),
                hypothesis_for(combo),
            ));
        }
    } else {
        // Driver order is the walk's own order, not a total rank. Compare as
        // a set. `total_limit` is a prefix of THAT walk, which the oracle
        // does not know, so a limited driver page must be a duplicate-free
        // subset of the filtered set, of size min(limit, |filtered|).
        let a: BTreeSet<_> = actual.iter().copied().collect();
        if actual.len() != a.len() {
            return Err(fail(
                combo,
                "dup",
                "driver order has no duplicate ids",
                "duplicates present",
                "src/query.rs Driver order walk",
            ));
        }
        let unlimited = {
            let mut c = combo.clone();
            c.limit = None;
            oracle.oracle(&c)
        };
        let e: BTreeSet<_> = unlimited.iter().copied().collect();
        if !actual.iter().all(|id| e.contains(id)) {
            return Err(fail(
                combo,
                "set",
                &format!("subset of {:?}", seqs(&unlimited)),
                &format!("set {:?}", seqs(&actual)),
                hypothesis_for(combo),
            ));
        }
        let want = combo.limit.unwrap_or(unlimited.len()).min(unlimited.len());
        if actual.len() != want {
            return Err(fail(
                combo,
                "set",
                &format!("driver page len {want} (limit {:?}, |filtered|={})", combo.limit, unlimited.len()),
                &format!("len {}", actual.len()),
                hypothesis_for(combo),
            ));
        }
    }
    let exhausted = actual.len() == expected.len();
    let last_done = drain.pages.last().is_some_and(|p| p.done);
    if last_done != exhausted && !(expected.is_empty() && last_done) {
        // empty: first page done=true and exhausted
    }
    if drain.pages.is_empty() {
        return Err(fail(combo, "done", "at least one page", "no pages", "next_page"));
    }
    if !last_done {
        return Err(fail(
            combo,
            "done",
            "done when oracle exhausted",
            "last page done=false",
            "src/query.rs:7305 finish_page done flag",
        ));
    }
    for (i, page) in drain.pages.iter().enumerate() {
        if i + 1 < drain.pages.len() && page.done {
            return Err(fail(
                combo,
                "done",
                "done only on last page",
                &format!("page {i} set done with more pages"),
                "src/query.rs:7305",
            ));
        }
    }

    // (4) prepare twice, identical pages
    let drain2 = drain_query(db, meta, combo).map_err(|e| {
        fail(combo, "repeat", "second prepare succeeds", &format!("{e:?}"), "prepare_query")
    })?;
    if ids_of(&drain2.rows) != actual {
        return Err(fail(
            combo,
            "repeat",
            "identical pages from two prepares",
            "mismatch",
            "src/query.rs prepare is not deterministic for this plan",
        ));
    }

    let first = drain.pages.first().unwrap();
    if expect_zero_primary(combo, first.driver) && first.work.primary_reads != 0 {
        return Err(fail(
            combo,
            "primary_reads",
            "primary_reads == 0 for index-side Ids",
            &format!("{} (driver={:?})", first.work.primary_reads, first.driver),
            "src/query.rs:4138 EntityCursor / emit_rows winner probe",
        ));
    }

    if depth == Depth::Fast {
        return Ok(());
    }

    // (5) tight budget on each used resource of the first page
    for resource in RESOURCES {
        let used = work_of(&first.work, resource);
        if used == 0 {
            continue;
        }
        let tight = set_budget(generous(), resource, used - 1);
        let outcome = with_prepared(db, meta, combo, |prep| {
            let mut q = prep.map_err(|e| format!("prepare {e:?}"))?;
            match q.next_page(combo.page, tight, || false) {
                Err(QueryError::BudgetExceeded { resource: r, .. }) if r == resource || true => {
                    match q.next_page(combo.page, generous(), || false) {
                        Ok(page) => {
                            if ids_of(&page.rows) != ids_of(&first.rows) {
                                Err("retry page != original first page".into())
                            } else {
                                Ok(())
                            }
                        }
                        Err(e) => Err(format!("retry after budget: {e:?}")),
                    }
                }
                // A tighter budget does not have to REFUSE the page. The
                // membership walk absorbs its own `ScalarPostings` /
                // `SpatialPostings` exhaustion on purpose -- the set becomes
                // `MembershipSet::Overflow` and the filter goes back to the
                // row-read path it used before that walk existed
                // (`src/query.rs:7866-7879`), so the page succeeds having
                // spent less. What the contract does say is that the answer
                // is the same one either way, so that is what is asserted.
                Ok(page) => {
                    if ids_of(&page.rows) != ids_of(&first.rows) {
                        Err(format!(
                            "tight {resource:?} limit={} page != original first page",
                            used - 1
                        ))
                    } else {
                        Ok(())
                    }
                }
                Err(e) => Err(format!("tight budget: {e:?}")),
            }
        });
        if let Err(msg) = outcome {
            return Err(fail(
                combo,
                "budget",
                "BudgetExceeded then identical retry",
                &msg,
                "src/query.rs:542 WorkMeter::charge / next_page must not move after failure",
            ));
        }
    }

    // (6) cancel at Nth check
    let ns: &[u32] = if depth == Depth::Full {
        &[1, 2, 3, 4, 5, 6, 7, 8]
    } else {
        &[1, 8]
    };
    for &n in ns {
        let outcome = with_prepared(db, meta, combo, |prep| {
            let mut q = prep.map_err(|e| format!("prepare {e:?}"))?;
            let mut checks = 0u32;
            match q.next_page(combo.page, generous(), || {
                checks += 1;
                checks == n
            }) {
                Err(QueryError::Cancelled) => match q.next_page(combo.page, generous(), || false) {
                    Ok(page) => {
                        if ids_of(&page.rows) != ids_of(&first.rows) {
                            Err("cancel retry page != original".into())
                        } else {
                            Ok(())
                        }
                    }
                    Err(e) => Err(format!("retry after cancel: {e:?}")),
                },
                Ok(page) => {
                    if ids_of(&page.rows) != ids_of(&first.rows) {
                        Err("uncancelled N produced a different page".into())
                    } else {
                        Ok(())
                    }
                }
                Err(e) => Err(format!("cancel N={n}: {e:?}")),
            }
        });
        if let Err(msg) = outcome {
            return Err(fail(
                combo,
                "cancel",
                "Cancelled then identical retry",
                &msg,
                "src/query.rs:489 check_cancelled / next_page must not move after cancel",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Depth {
    Fast,
    Full,
}

fn hypothesis_for(combo: &Combo) -> &'static str {
    if combo.filters.iter().any(OwnedFilter::is_geometry) {
        return "src/query.rs:6083 geometry_predicate_matches / spatial_geometry.rs:1094 intersects";
    }
    if matches!(combo.order, OwnedOrder::ApproxVector(_)) {
        return "src/query.rs ApproximateVector shortlist+rerank (ef=4096) vs exact f32";
    }
    if matches!(combo.order, OwnedOrder::Score(ScoreKind::DivByScore)) {
        return "src/query.rs:6220 Div by zero → NaN; compare_rank_value NaN last (query.rs:1389)";
    }
    if matches!(combo.order, OwnedOrder::Score(ScoreKind::DistanceMissing)) {
        return "src/query.rs:6187 Distance leaf missing loc → INFINITY";
    }
    if matches!(combo.order, OwnedOrder::Distance) {
        return "src/query.rs:6280 Distance order drops missing loc (rank_candidate None)";
    }
    if combo.filters.iter().any(OwnedFilter::is_key) {
        return "src/query.rs:2599 Key filter / DriverPlan::Keys";
    }
    if matches!(combo.order, OwnedOrder::KindAsc) {
        return "src/scalar_key.rs:2 text UTF-8 order; nullish key [0] sorts first";
    }
    if matches!(combo.order, OwnedOrder::ExactVector(_)) {
        return "src/vector_indexes.rs:215 score_f32_pre; locator ordinal after alter_collection (query.rs:980)";
    }
    if combo.filters.iter().any(|f| matches!(f, OwnedFilter::IsNullKind | OwnedFilter::IsMissingScore)) {
        return "src/query.rs:5289 IsNull vs IsMissing (Null vs Missing field states)";
    }
    "src/query.rs combined filter ∩ rank"
}

fn filter_catalog() -> Vec<OwnedFilter> {
    vec![
        OwnedFilter::EqKind("alpha"),
        OwnedFilter::EqKind("bravo"),
        OwnedFilter::EqKind("echo"),
        OwnedFilter::RangeBorn { lo: 1940, hi: 1960 },
        OwnedFilter::RangeBorn { lo: 1970, hi: 1995 },
        OwnedFilter::RangeBorn { lo: 2000, hi: 2020 },
        OwnedFilter::EqFlag(true),
        OwnedFilter::EqFlag(false),
        OwnedFilter::IsNullKind,
        OwnedFilter::IsMissingScore,
        OwnedFilter::PointRadius {
            lon: CLUSTERS[0].0,
            lat: CLUSTERS[0].1,
            metres: 3_000.0,
        },
        OwnedFilter::PointRadius {
            lon: CLUSTERS[1].0,
            lat: CLUSTERS[1].1,
            metres: 500.0,
        },
        OwnedFilter::PointBbox {
            west: 144.90,
            east: 145.02,
            south: -37.86,
            north: -37.76,
        },
        OwnedFilter::PointBbox {
            west: 10.0,
            east: 11.0,
            south: 10.0,
            north: 11.0,
        },
        OwnedFilter::GeomIntersects,
        OwnedFilter::GeomWithin,
        OwnedFilter::GeomContains,
        OwnedFilter::GeomDWithin { metres: 400.0 },
        OwnedFilter::TextAny(ANY_TERM),
        OwnedFilter::TextAll(ALL_TERMS),
        OwnedFilter::TextPhrase(PHRASE),
        OwnedFilter::KeyRange {
            lo: "k0100",
            hi: "k0400",
        },
        OwnedFilter::KeyRange {
            lo: "k1800",
            hi: "k1990",
        },
    ]
}

fn order_catalog() -> Vec<OwnedOrder> {
    vec![
        OwnedOrder::EntityId,
        OwnedOrder::BornAsc,
        OwnedOrder::BornDesc,
        OwnedOrder::KindAsc,
        OwnedOrder::Driver,
        OwnedOrder::Distance,
        OwnedOrder::Bm25,
        OwnedOrder::ExactVector(VectorMetric::Cosine),
        OwnedOrder::ExactVector(VectorMetric::SquaredL2),
        OwnedOrder::ExactVector(VectorMetric::NegativeDot),
        OwnedOrder::ApproxVector(VectorMetric::Cosine),
        OwnedOrder::ApproxVector(VectorMetric::SquaredL2),
        OwnedOrder::ApproxVector(VectorMetric::NegativeDot),
        OwnedOrder::Score(ScoreKind::DivByScore),
        OwnedOrder::Score(ScoreKind::DistanceMissing),
        OwnedOrder::Score(ScoreKind::HybridBm25Vec),
        OwnedOrder::Score(ScoreKind::BornMinusDist),
    ]
}

fn hand_picked() -> Vec<Combo> {
    let c = |name: &str, why: &'static str, filters: Vec<OwnedFilter>, order: OwnedOrder, driver: OwnedDriver, proj: OwnedProj, page: usize, limit: Option<usize>| Combo {
        name: name.into(),
        why,
        filters,
        order,
        driver,
        proj,
        page,
        limit,
    };
    vec![
        c("hp01_key_auto_row_answered", "Key range under Auto is answered from the row's own key field (a named per-candidate read), equal to brute force", vec![OwnedFilter::KeyRange { lo: "k0100", hi: "k0400" }], OwnedOrder::EntityId, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp02_key_keys_id", "Mapping-keyspace walk vs brute-force key compare", vec![OwnedFilter::KeyRange { lo: "k0100", hi: "k0400" }], OwnedOrder::EntityId, OwnedDriver::Keys, OwnedProj::Ids, 7, None),
        c("hp03_key_entities_row_answered", "Key range under the entity driver is a row predicate; rows equal brute force", vec![OwnedFilter::KeyRange { lo: "k0100", hi: "k0400" }], OwnedOrder::EntityId, OwnedDriver::Entities, OwnedProj::Ids, 7, None),
        c("hp04_ismissing_score", "IsMissing is a field-state, not null; score was omitted not nulled", vec![OwnedFilter::IsMissingScore], OwnedOrder::EntityId, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp05_isnull_kind", "JSON null kind vs missing; 5% rows store explicit null", vec![OwnedFilter::IsNullKind], OwnedOrder::KindAsc, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp06_same_index_contradict", "kind Eq alpha AND kind IsNull: same index, empty intersection", vec![OwnedFilter::EqKind("alpha"), OwnedFilter::IsNullKind], OwnedOrder::EntityId, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp07_same_index_born_fold", "two born ranges on one index must fold, not double-filter via the row", vec![OwnedFilter::RangeBorn { lo: 1940, hi: 1980 }, OwnedFilter::RangeBorn { lo: 1970, hi: 1995 }], OwnedOrder::BornAsc, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp08_three_scalars", "kind ∩ born ∩ flag: three index families, Auto driver pick", vec![OwnedFilter::EqKind("alpha"), OwnedFilter::RangeBorn { lo: 1950, hi: 1975 }, OwnedFilter::EqFlag(true)], OwnedOrder::BornDesc, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp09_geom_and_point", "geometry refine is row-side; point is posting-side; conjunction", vec![OwnedFilter::GeomIntersects, OwnedFilter::PointRadius { lon: CLUSTERS[0].0, lat: CLUSTERS[0].1, metres: 3_000.0 }], OwnedOrder::EntityId, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp10_geom_contains", "Contains is planar ST_Contains; tiny query triangle vs 50-500 m plots", vec![OwnedFilter::GeomContains], OwnedOrder::EntityId, OwnedDriver::Filter(0), OwnedProj::Ids, 7, None),
        c("hp11_geom_within", "Within is planar; large query polygon around cluster 0", vec![OwnedFilter::GeomWithin], OwnedOrder::Driver, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp12_geom_dwithin", "DWithin is spheroidal; 400 m from cluster 0 point", vec![OwnedFilter::GeomDWithin { metres: 400.0 }], OwnedOrder::EntityId, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp13_two_text_same_index", "Any + Phrase on the same text index: membership ∩ phrase refine", vec![OwnedFilter::TextAny(ANY_TERM), OwnedFilter::TextPhrase(PHRASE)], OwnedOrder::Bm25, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp14_text_all_zero", "All of 'river harbour' may admit 0 rows (contradictory pair)", vec![OwnedFilter::TextAll(ALL_TERMS)], OwnedOrder::Bm25, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp15_bbox_empty", "bbox in the Atlantic admits 0 rows", vec![OwnedFilter::PointBbox { west: 10.0, east: 11.0, south: 10.0, north: 11.0 }], OwnedOrder::Distance, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp16_vec_cos_kind", "Exact Cosine order restricted by kind Eq; missing emb dropped", vec![OwnedFilter::EqKind("alpha")], OwnedOrder::ExactVector(VectorMetric::Cosine), OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp17_ann_eq_exact_cos", "ApproximateVector ef=4096 must equal exact Cosine on this corpus", vec![], OwnedOrder::ApproxVector(VectorMetric::Cosine), OwnedDriver::Order, OwnedProj::Ids, 7, Some(5)),
        c("hp18_ann_l2", "ANN SquaredL2 rerank vs exact f32 L2 after alter_collection ordinal shift", vec![], OwnedOrder::ApproxVector(VectorMetric::SquaredL2), OwnedDriver::Auto, OwnedProj::Ids, 7, Some(5)),
        c("hp19_ann_ndot", "ANN NegativeDot; quantized shortlist then authoritative sidecar", vec![], OwnedOrder::ApproxVector(VectorMetric::NegativeDot), OwnedDriver::Auto, OwnedProj::Ids, 7, Some(5)),
        c("hp20_score_div_zero", "Lit(1)/score; missing and 0.0 → NaN last under ascending", vec![], OwnedOrder::Score(ScoreKind::DivByScore), OwnedDriver::Entities, OwnedProj::Ids, 7, None),
        c("hp21_score_dist_missing", "Distance leaf: missing loc scores +inf, still in the answer", vec![], OwnedOrder::Score(ScoreKind::DistanceMissing), OwnedDriver::Auto, OwnedProj::Ids, 7, Some(5)),
        c("hp22_score_hybrid", "Bm25 + VectorSimilarity; missing vec is -inf, non-match BM25 is 0", vec![OwnedFilter::EqFlag(true)], OwnedOrder::Score(ScoreKind::HybridBm25Vec), OwnedDriver::Auto, OwnedProj::Ids, 7, Some(5)),
        c("hp23_score_born_dist", "born − distance; missing loc → born − inf = -inf", vec![], OwnedOrder::Score(ScoreKind::BornMinusDist), OwnedDriver::Order, OwnedProj::Ids, 7, Some(5)),
        c("hp24_driver_radius", "Driver order on a point filter is cell order: compare as a set", vec![OwnedFilter::PointRadius { lon: CLUSTERS[0].0, lat: CLUSTERS[0].1, metres: 3_000.0 }], OwnedOrder::Driver, OwnedDriver::Auto, OwnedProj::Ids, 7, None),
        c("hp25_distance_drops_missing", "Distance order omits missing loc; Score Distance does not", vec![], OwnedOrder::Distance, OwnedDriver::Auto, OwnedProj::Ids, 7, Some(5)),
        c("hp26_fields_after_alter", "Fields projection including missing_field and emb after inserting tag before emb", vec![OwnedFilter::EqKind("bravo")], OwnedOrder::EntityId, OwnedDriver::Auto, OwnedProj::Fields, 7, Some(5)),
        c("hp27_page1_limit5", "page_size=1 with total_limit=5: done at 5, no dups, prefix of born desc", vec![OwnedFilter::RangeBorn { lo: 1940, hi: 1990 }], OwnedOrder::BornDesc, OwnedDriver::Auto, OwnedProj::Ids, 1, Some(5)),
        c("hp28_geom_drive_vec_order", "Filter(0)=geometry driving, ExactVector ranking: row refine + sidecar", vec![OwnedFilter::GeomIntersects], OwnedOrder::ExactVector(VectorMetric::Cosine), OwnedDriver::Filter(0), OwnedProj::Ids, 7, Some(5)),
        c("hp29_order_drives_score", "CandidateDriver::Order on Score is an entity scan ranked by the expression", vec![OwnedFilter::EqFlag(false)], OwnedOrder::Score(ScoreKind::DivByScore), OwnedDriver::Order, OwnedProj::Ids, 7, Some(5)),
        c("hp30_exact_vec_fields_layout", "ExactVector + Fields(emb) after layout insert-before-emb (locator ordinal)", vec![], OwnedOrder::ExactVector(VectorMetric::Cosine), OwnedDriver::Order, OwnedProj::Fields, 7, Some(5)),
    ]
}

fn sample_combos(n: usize, seed: u64) -> Vec<Combo> {
    let mut rng = Rng(seed);
    let filters = filter_catalog();
    let orders = order_catalog();
    let pages = [1usize, 7, 8192];
    let limits = [None, Some(1usize), Some(5usize)];
    let mut out = Vec::with_capacity(n);
    let mut seen = HashSet::new();
    let mut spins = 0;
    while out.len() < n && spins < n * 20 {
        spins += 1;
        let nf = rng.usize(4);
        let mut fs = Vec::with_capacity(nf);
        for _ in 0..nf {
            fs.push(filters[rng.usize(filters.len())].clone());
        }
        if fs.iter().filter(|f| f.is_key()).count() > 1 {
            continue;
        }
        let order = orders[rng.usize(orders.len())].clone();
        let driver = if fs.iter().any(OwnedFilter::is_key) {
            match rng.usize(3) {
                0 => OwnedDriver::Keys,
                1 => OwnedDriver::Auto,
                _ => OwnedDriver::Entities,
            }
        } else {
            match rng.usize(6) {
                0 => OwnedDriver::Auto,
                1 => OwnedDriver::Entities,
                2 => OwnedDriver::Order,
                3 => OwnedDriver::Keys,
                4 if !fs.is_empty() => OwnedDriver::Filter(rng.usize(fs.len())),
                _ => OwnedDriver::Auto,
            }
        };
        let proj = if rng.chance(0.25) {
            OwnedProj::Fields
        } else {
            OwnedProj::Ids
        };
        let page = pages[rng.usize(3)];
        let limit = limits[rng.usize(3)];
        let label = format!("{fs:?}{order:?}{driver:?}{proj:?}{page}{limit:?}");
        let _ = order.label();
        let key = format!(
            "{}-{}-{:?}-{:?}-{}-{:?}",
            fs.iter().map(OwnedFilter::label).collect::<Vec<_>>().join("+"),
            order.label(),
            driver,
            proj,
            page,
            limit
        );
        if !seen.insert(key.clone()) {
            continue;
        }
        let _ = label;
        out.push(Combo {
            name: format!("r{:04}", out.len()),
            why: "seeded random sample",
            filters: fs,
            order,
            driver,
            proj,
            page,
            limit,
        });
    }
    out
}

fn exhaustive_combos() -> Vec<Combo> {
    let mut out = Vec::new();
    for (fi, f) in filter_catalog().into_iter().enumerate() {
        for (oi, o) in order_catalog().into_iter().enumerate() {
            out.push(Combo {
                name: format!("ex_f{fi}_o{oi}"),
                why: "exhaustive single-filter × single-order × Auto",
                filters: vec![f.clone()],
                order: o,
                driver: OwnedDriver::Auto,
                proj: OwnedProj::Ids,
                page: 7,
                limit: None,
            });
        }
    }
    out
}

fn run_list(
    db: &Database,
    meta: &Meta,
    oracle: &Oracle<'_>,
    combos: &[Combo],
    depth: Depth,
    failures: &mut Vec<Failure>,
    ran: &mut usize,
) {
    for (i, combo) in combos.iter().enumerate() {
        *ran += 1;
        if let Err(f) = run_one(db, meta, oracle, combo, depth) {
            eprintln!(
                "FAIL {} :: {} :: {} :: {} :: why={}",
                f.name, f.kind, f.combo, f.actual, combo.why
            );
            failures.push(f);
        }
        if i % 50 == 0 {
            eprintln!("progress {}/{} failures={}", i + 1, combos.len(), failures.len());
        }
    }
}

/// The whole surface: hand-picked, exhaustive, a 3,000-combination seeded
/// sample, a reopen pass and the snapshot case. It runs for about 40 minutes,
/// so it is `#[ignore]`d and the lean tests below cover the same assertions on
/// the sets that finish quickly.
///
/// Run it with:
/// `cargo test --features compact-cells,sqlite-balance,keyspace-append,slotref-split --test query_combinations -- --test-threads=1 --ignored --exact query_engine_surface_combinations`
#[ignore = "about 40 minutes; run with -- --ignored"]
#[test]
fn query_engine_surface_combinations() {
    let start = std::time::SystemTime::now();
    eprintln!("START {:?}", chrono_now());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let meta = build_fixture(&path);
    eprintln!(
        "fixture live={} path={} elapsed={:?}",
        meta.live.len(),
        path.display(),
        start.elapsed().unwrap()
    );
    let db = Database::open(&meta.path, cfg()).unwrap();
    let oracle = Oracle::new(&meta, &db);

    let mut failures = Vec::new();
    let mut ran = 0usize;

    let picked = hand_picked();
    eprintln!("hand-picked {}", picked.len());
    run_list(&db, &meta, &oracle, &picked, Depth::Full, &mut failures, &mut ran);

    let exhaustive = exhaustive_combos();
    eprintln!("exhaustive {}", exhaustive.len());
    run_list(&db, &meta, &oracle, &exhaustive, Depth::Full, &mut failures, &mut ran);

    let sample = sample_combos(3_000, SEED ^ 0xC0FF_EE);
    eprintln!("sample {}", sample.len());
    run_list(&db, &meta, &oracle, &sample, Depth::Fast, &mut failures, &mut ran);

    // reopen pass: 200 random combinations, identical results
    let reopen_set: Vec<Combo> = sample.iter().take(200).cloned().collect();
    let mut before = Vec::new();
    for c in &reopen_set {
        if c.expects_prepare_refuse() {
            before.push(None);
            continue;
        }
        before.push(Some(oracle.oracle(c)));
    }
    drop(oracle);
    drop(db);
    let db = Database::open(&meta.path, cfg()).unwrap();
    let oracle = Oracle::new(&meta, &db);
    for (c, prev) in reopen_set.iter().zip(before.iter()) {
        ran += 1;
        match (prev, run_one(&db, &meta, &oracle, c, Depth::Fast)) {
            (Some(old), Ok(())) => {
                let now = oracle.oracle(c);
                if now != *old {
                    failures.push(fail(
                        c,
                        "reopen",
                        "identical oracle after reopen",
                        "oracle changed",
                        "reopen must not change committed bytes (L6/L8)",
                    ));
                }
            }
            (_, Err(f)) => failures.push(f),
            (None, Ok(())) => {}
        }
    }

    // (8) snapshot: remaining pages of a prepared query ignore a concurrent commit
    drop(oracle);
    drop(db);
    {
        let combo = Combo {
            name: "snapshot_remaining".into(),
            why: "prepared query is a snapshot; concurrent writer must not move remaining pages",
            filters: vec![OwnedFilter::RangeBorn { lo: 1940, hi: 2010 }],
            order: OwnedOrder::BornAsc,
            driver: OwnedDriver::Auto,
            proj: OwnedProj::Ids,
            page: 7,
            limit: None,
        };
        let mut writer = Database::open(&meta.path, cfg()).unwrap();
        let snapshot = Database::open_snapshot(&meta.path, cfg()).unwrap();
        let expected = Oracle::new(&meta, &snapshot).oracle(&combo);
        let remaining_ok = with_prepared(&snapshot, &meta, &combo, |prep| {
            let mut q = prep.unwrap();
            let first = q.next_page(7, generous(), || false).unwrap();
            let mut rows = first.rows;
            for i in 0..50 {
                writer
                    .put(
                        meta.collection,
                        &format!("zsnap{i:02}"),
                        &json!({
                            "name": "snap extra",
                            "body": "flood river bank",
                            "born": 1955,
                            "score": 1.0,
                            "flag": true,
                            "kind": "alpha",
                        }),
                    )
                    .unwrap();
            }
            for row in meta
                .live
                .iter()
                .filter(|r| r.born >= 1940 && r.born <= 2010)
                .take(20)
            {
                writer.delete(meta.collection, &row.key).unwrap();
            }
            writer.commit().unwrap();
            loop {
                let page = q.next_page(7, generous(), || false).unwrap();
                rows.extend(page.rows);
                if page.done {
                    break;
                }
            }
            ids_of(&rows)
        });
        ran += 1;
        if remaining_ok != expected {
            failures.push(fail(
                &combo,
                "snapshot",
                &format!("snapshot ids {:?}", seqs(&expected)),
                &format!("got {:?}", seqs(&remaining_ok)),
                "src/query.rs PreparedQuery holds the snapshot; L6 readers keep the acknowledged commit",
            ));
        }
        let live_now = with_prepared(&writer, &meta, &combo, |prep| {
            let mut q = prep.unwrap();
            let mut rows = Vec::new();
            loop {
                let page = q.next_page(8192, generous(), || false).unwrap();
                rows.extend(page.rows);
                if page.done {
                    break;
                }
            }
            ids_of(&rows)
        });
        if live_now == expected {
            failures.push(fail(
                &combo,
                "snapshot",
                "newly prepared query on writer must see the 50 inserts / 20 deletes",
                "writer query identical to pre-commit snapshot",
                "src/query.rs prepare_query on the writer after commit",
            ));
        }
        drop(snapshot);
        drop(writer);
    }

    let elapsed = start.elapsed().unwrap();
    eprintln!(
        "END {:?} ran={ran} failed={} elapsed={elapsed:?}",
        chrono_now(),
        failures.len()
    );
    for f in &failures {
        eprintln!(
            "FAIL-DETAIL name={} kind={} hypothesis={} combo={}",
            f.name, f.kind, f.hypothesis, f.combo
        );
        eprintln!("  expected: {}", trunc(&f.expected, 400));
        eprintln!("  actual:   {}", trunc(&f.actual, 400));
    }
    // Persist a machine-readable sidecar next to the test output for the report.
    let sidecar = format!(
        "ran={ran}\nfailed={}\nelapsed_s={}\n",
        failures.len(),
        elapsed.as_secs()
    );
    let _ = std::fs::write("<scratch>", sidecar);
    let mut lines = String::new();
    for f in &failures {
        lines.push_str(&format!(
            "NAME {}\nKIND {}\nHYP {}\nCOMBO {}\nEXP {}\nACT {}\n---\n",
            f.name, f.kind, f.hypothesis, f.combo, f.expected, f.actual
        ));
    }
    let _ = std::fs::write("<scratch>", lines);
    assert!(
        failures.is_empty(),
        "{} combination(s) failed (ran {ran}); see stderr and <scratch>",
        failures.len()
    );
}

fn trunc(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    format!("unix={t}")
}

/// Distinct-failure named tests are appended after the first run when a
/// combination is shown to disagree with the oracle. They rebuild the same
/// seeded fixture so the failure stays visible without the 3,000-combo loop.
fn run_named(combo: Combo) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let meta = build_fixture(&path);
    let db = Database::open(&meta.path, cfg()).unwrap();
    let oracle = Oracle::new(&meta, &db);
    if let Err(f) = run_one(&db, &meta, &oracle, &combo, Depth::Full) {
        panic!(
            "{} [{}] {}\nexpected: {}\nactual: {}\nhypothesis: {}",
            f.name, f.kind, f.combo, f.expected, f.actual, f.hypothesis
        );
    }
}

#[test]
fn hand_picked_key_filter_answers_under_any_driver() {
    run_named(hand_picked().into_iter().find(|c| c.name == "hp01_key_auto_row_answered").unwrap());
    run_named(hand_picked().into_iter().find(|c| c.name == "hp03_key_entities_row_answered").unwrap());
}

#[test]
fn hand_picked_score_div_by_zero_nan_last() {
    run_named(hand_picked().into_iter().find(|c| c.name == "hp20_score_div_zero").unwrap());
}

#[test]
fn hand_picked_approx_vector_equals_exact() {
    run_named(hand_picked().into_iter().find(|c| c.name == "hp17_ann_eq_exact_cos").unwrap());
}

#[test]
fn hand_picked_layout_shift_exact_vector_fields() {
    run_named(hand_picked().into_iter().find(|c| c.name == "hp30_exact_vec_fields_layout").unwrap());
}

/// Distinct failure A: a BudgetExceeded Distance page puts the mutated
/// nearest walk back (`query.rs:7584` take, `query.rs:8017` assign even on
/// Err), so the unlimited retry is not the original first page.
#[test]
fn fail_budget_retry_moves_nearest_walk() {
    run_named(
        hand_picked()
            .into_iter()
            .find(|c| c.name == "hp25_distance_drops_missing")
            .unwrap(),
    );
}

/// Distinct failure B: a BudgetExceeded geometry Driver-order page commits
/// `geometry_seen` (`query.rs:7958`) before `emit_rows` succeeds, so the
/// unlimited retry skips entities the failed page had already marked.
#[test]
fn fail_budget_retry_moves_geometry_driver_seen() {
    run_named(
        hand_picked()
            .into_iter()
            .find(|c| c.name == "hp11_geom_within")
            .unwrap(),
    );
}


/// The 30 hand-picked combinations at `Depth::Full` -- every assertion the
/// long test makes, on the set that was chosen by hand to cover one engine
/// decision each. Extracted out of `query_engine_surface_combinations` so the
/// sharp end of that test stays runnable without the 3,000-combination sample.
#[test]
fn hand_picked_surface_combinations() {
    run_set(&hand_picked(), "hand-picked");
}

/// Every single filter crossed with every single order under the Auto driver,
/// at `Depth::Full`. Also extracted out of the long test.
#[test]
fn exhaustive_single_filter_order_combinations() {
    run_set(&exhaustive_combos(), "exhaustive");
}

fn run_set(combos: &[Combo], label: &str) {
    let start = std::time::SystemTime::now();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let meta = build_fixture(&path);
    let db = Database::open(&meta.path, cfg()).unwrap();
    let oracle = Oracle::new(&meta, &db);
    let mut failures = Vec::new();
    let mut ran = 0usize;
    run_list(&db, &meta, &oracle, combos, Depth::Full, &mut failures, &mut ran);
    eprintln!(
        "{label} ran={ran} failed={} elapsed={:?}",
        failures.len(),
        start.elapsed().unwrap()
    );
    for f in &failures {
        eprintln!(
            "FAIL-DETAIL name={} kind={} hypothesis={} combo={}",
            f.name, f.kind, f.hypothesis, f.combo
        );
        eprintln!("  expected: {}", trunc(&f.expected, 400));
        eprintln!("  actual:   {}", trunc(&f.actual, 400));
    }
    assert!(failures.is_empty(), "{} of {ran} {label} combination(s) failed", failures.len());
}

/// Distinct failure C, found by auditing `next_page` for the same class as A
/// and B: a page served out of the HELD RUN popped its winners off `self.run`
/// before `emit_rows`, and `emit_rows` still charges `OutputBytes` for every
/// row it builds (`src/query.rs:7834`) and the winner probe's `PrimaryReads`.
/// A budget failure there dropped the popped winners on the floor, so the
/// unlimited retry returned the slice AFTER the page that failed.
///
/// The shape: a point-radius filter under `QueryOrder::EntityId`. The spatial
/// cell walk is not in id order, so `driver_walks_in_rank_order` is
/// `RankWalk::No` and the page holds back everything it ranked past what it
/// returns (`keeps_a_run`); page 2 is then served entirely out of that hold.
#[test]
fn fail_budget_retry_moves_held_run() {
    let combo = Combo {
        name: "run_hold_page2".into(),
        why: "a page served from the held run must not consume it until its rows are final",
        filters: vec![OwnedFilter::PointRadius {
            lon: CLUSTERS[0].0,
            lat: CLUSTERS[0].1,
            metres: 3_000.0,
        }],
        order: OwnedOrder::EntityId,
        driver: OwnedDriver::Auto,
        proj: OwnedProj::Ids,
        page: 7,
        limit: None,
    };
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let meta = build_fixture(&path);
    let db = Database::open(&meta.path, cfg()).unwrap();

    // Page 1 walks and fills the hold; page 2 is served from it.
    let (page_one, page_two, page_two_bytes) = with_prepared(&db, &meta, &combo, |prep| {
        let mut q = prep.unwrap();
        let first = q.next_page(combo.page, generous(), || false).unwrap();
        let second = q.next_page(combo.page, generous(), || false).unwrap();
        (
            ids_of(&first.rows),
            ids_of(&second.rows),
            second.work.output_bytes,
        )
    });
    assert_eq!(page_one.len(), combo.page, "page 1 must be full: {page_one:?}");
    assert!(!page_two.is_empty(), "the fixture must give this query a second page");
    assert!(
        page_two_bytes > 0,
        "page 2 must charge output bytes for the budget to be able to fail it"
    );

    // The same page 2, refused one output byte short, then retried unlimited.
    let retried = with_prepared(&db, &meta, &combo, |prep| {
        let mut q = prep.unwrap();
        let first = q.next_page(combo.page, generous(), || false).unwrap();
        assert_eq!(ids_of(&first.rows), page_one);
        let tight = set_budget(generous(), WorkResource::OutputBytes, page_two_bytes - 1);
        match q.next_page(combo.page, tight, || false) {
            Err(QueryError::BudgetExceeded {
                resource: WorkResource::OutputBytes,
                ..
            }) => {}
            Ok(page) => panic!(
                "expected BudgetExceeded on OutputBytes limit={}, got {} rows",
                page_two_bytes - 1,
                page.rows.len()
            ),
            Err(other) => panic!("expected BudgetExceeded, got {other:?}"),
        }
        ids_of(&q.next_page(combo.page, generous(), || false).unwrap().rows)
    });
    assert_eq!(
        seqs(&retried),
        seqs(&page_two),
        "a failed page served from the held run must leave the run where it found it"
    );
}
