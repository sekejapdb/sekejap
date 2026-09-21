//! The 2,000-row fixture the three SQL suites share, in the style of
//! `tests/query_combinations.rs`: a deterministic generator, one collection,
//! every index family, and a small typed graph on top.
//!
//! The schema is `battle50k`'s, so a statement written for the battery runs
//! here unchanged -- which is what lets `lang/tests/sql_explain.rs` assert the
//! driver and the counters of the battery's own cases without a 50,000-row
//! corpus.
#![allow(dead_code)]

use sekejap_core::{
    collections::{
        CollectionId, CollectionOptions, Database, EdgeTypeId, Geom, GraphContextId, IndexId,
    },
    spatial_math::Point,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const ROWS: usize = 2_000;
pub const DIM: usize = 8;
const SEED: u64 = 0x5351_4C53_4C49_4345;

pub const KINDS: [&str; 8] = [
    "depot", "farm", "home", "mill", "park", "port", "school", "shop",
];
pub const VOCAB: [&str; 12] = [
    "kebun", "sekolah", "jembatan", "bengkel", "desa", "kopi", "sawah", "danau", "pasar", "kantor",
    "hutan", "warung",
];
/// Cluster centres, in the same part of the world `battle50k`'s corpus uses.
const CLUSTERS: [(f64, f64); 4] = [
    (106.82, -6.17),
    (107.61, -6.91),
    (110.42, -6.97),
    (112.75, -7.25),
];

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn usize(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() as usize) % n
        }
    }
    pub fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 / ((1u64 << 53) as f64)
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: String,
    pub name: String,
    pub desc: String,
    pub text: String,
    pub born: i64,
    pub kind: String,
    pub lon: f64,
    pub lat: f64,
    pub plot: Geom,
    pub emb: Vec<f32>,
    /// `None` is written as a JSON null, which is present-but-null.
    pub score: Option<f64>,
    pub flag: bool,
    /// Absent from the document entirely when `None`, so IS MISSING has
    /// something to find.
    pub tag: Option<String>,
}

pub struct Fixture {
    pub path: PathBuf,
    pub db: Database,
    pub place: CollectionId,
    pub rows: Vec<Row>,
    pub keys: Vec<String>,
    pub context: GraphContextId,
    pub near: EdgeTypeId,
    pub index: Indexes,
}

#[derive(Clone, Copy)]
pub struct Indexes {
    pub text: IndexId,
    pub born: IndexId,
    pub kind: IndexId,
    pub score: IndexId,
    pub flag: IndexId,
    pub loc: IndexId,
    pub plot: IndexId,
    pub emb_exact: IndexId,
    pub emb_ann: IndexId,
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>();
    if norm == 0.0 {
        v[0] = 1.0;
        return v;
    }
    let scale = norm.sqrt() as f32;
    for x in &mut v {
        *x /= scale;
    }
    v
}

fn square(lon: f64, lat: f64, metres: f64) -> Geom {
    let dlat = metres / 111_320.0;
    let dlon = metres / (111_320.0 * lat.to_radians().cos().abs().max(0.2));
    Geom::Polygon(vec![vec![
        [lon - dlon, lat - dlat],
        [lon + dlon, lat - dlat],
        [lon + dlon, lat + dlat],
        [lon - dlon, lat + dlat],
        [lon - dlon, lat - dlat],
    ]])
}

pub fn geom_json(geom: &Geom) -> Value {
    match geom {
        Geom::Point(x, y) => json!({"type": "Point", "coordinates": [x, y]}),
        Geom::LineString(c) => json!({"type": "LineString", "coordinates": c}),
        Geom::Polygon(rings) => json!({"type": "Polygon", "coordinates": rings}),
        Geom::MultiPoint(c) => json!({"type": "MultiPoint", "coordinates": c}),
        Geom::MultiLineString(rs) => json!({"type": "MultiLineString", "coordinates": rs}),
        Geom::MultiPolygon(ps) => json!({"type": "MultiPolygon", "coordinates": ps}),
    }
}

pub fn vector_literal(v: &[f32]) -> String {
    let mut out = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{x:?}"));
    }
    out.push(']');
    out
}

fn generate(i: usize, rng: &mut Rng) -> Row {
    let (clon, clat) = CLUSTERS[i % CLUSTERS.len()];
    let lon = clon + (rng.f64() - 0.5) * 0.4;
    let lat = clat + (rng.f64() - 0.5) * 0.3;
    let name = format!(
        "{} {}",
        VOCAB[rng.usize(VOCAB.len())],
        VOCAB[rng.usize(VOCAB.len())]
    );
    let words: Vec<&str> = (0..6).map(|_| VOCAB[rng.usize(VOCAB.len())]).collect();
    let desc = words.join(" ");
    let mut emb = vec![0.0f32; DIM];
    emb[i % DIM] = 1.0;
    for x in &mut emb {
        *x += (rng.f64() as f32 - 0.5) * 0.1;
    }
    Row {
        key: format!("k{i:05}"),
        text: format!("{name} {desc}"),
        name,
        desc,
        born: 19_500_101 + (i as i64 % 700) * 100,
        kind: KINDS[i % KINDS.len()].to_owned(),
        lon,
        lat,
        plot: square(lon, lat, 200.0 + rng.f64() * 600.0),
        emb: unit(emb),
        score: if i % 17 == 0 {
            None
        } else {
            Some((rng.f64() * 100.0).round() / 4.0)
        },
        flag: i % 3 == 0,
        tag: if i % 5 == 0 {
            None
        } else {
            Some(VOCAB[rng.usize(VOCAB.len())].to_owned())
        },
    }
}

fn document(row: &Row) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("key".into(), json!(row.key));
    object.insert("name".into(), json!(row.name));
    object.insert("descr".into(), json!(row.desc));
    object.insert("text".into(), json!(row.text));
    object.insert("born".into(), json!(row.born));
    object.insert("kind".into(), json!(row.kind));
    object.insert(
        "loc".into(),
        json!({"type": "Point", "coordinates": [row.lon, row.lat]}),
    );
    object.insert("plot".into(), geom_json(&row.plot));
    object.insert("emb".into(), json!(row.emb));
    object.insert(
        "score".into(),
        match row.score {
            Some(value) => json!(value),
            None => Value::Null,
        },
    );
    object.insert("flag".into(), json!(row.flag));
    if let Some(tag) = &row.tag {
        object.insert("tag".into(), json!(tag));
    }
    Value::Object(object)
}

fn config() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// Build the fixture under `dir`, which the caller owns.
pub fn build(dir: &Path) -> Fixture {
    let _ = std::fs::remove_dir_all(dir);
    let mut db = Database::create(dir, config()).unwrap();
    let place = db
        .create_collection(
            "place",
            vec![
                ("key".into(), Kind::Text),
                ("name".into(), Kind::Text),
                ("descr".into(), Kind::Text),
                ("text".into(), Kind::Text),
                ("born".into(), Kind::Int),
                ("kind".into(), Kind::Text),
                ("loc".into(), Kind::Point),
                ("plot".into(), Kind::Geo),
                ("emb".into(), Kind::Vector(DIM)),
                ("score".into(), Kind::Real),
                ("flag".into(), Kind::Bool),
                ("tag".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut rng = Rng(SEED);
    let mut rows = Vec::with_capacity(ROWS);
    for i in 0..ROWS {
        let row = generate(i, &mut rng);
        let id = db.put(place, &row.key, &document(&row)).unwrap();
        assert_eq!(id.sequence, (i + 1) as u64, "put order is file order");
        rows.push(row);
        if (i + 1) % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    let index = Indexes {
        text: db.create_text_index(place, "place_text", "text").unwrap(),
        born: db
            .create_scalar_index(place, "place_born", "born", false)
            .unwrap(),
        kind: db
            .create_scalar_index(place, "place_kind", "kind", false)
            .unwrap(),
        score: db
            .create_scalar_index(place, "place_score", "score", false)
            .unwrap(),
        flag: db
            .create_scalar_index(place, "place_flag", "flag", false)
            .unwrap(),
        loc: db.create_point_index(place, "place_loc", "loc").unwrap(),
        plot: db
            .create_geometry_index(place, "place_plot", "plot")
            .unwrap(),
        emb_exact: db
            .create_exact_vector_index(place, "place_emb_exact", "emb")
            .unwrap(),
        emb_ann: db
            .create_quantized_vector_index(place, "place_emb_ann", "emb")
            .unwrap(),
    };
    db.commit().unwrap();
    for id in [
        index.text,
        index.born,
        index.kind,
        index.score,
        index.flag,
        index.loc,
        index.plot,
        index.emb_exact,
        index.emb_ann,
    ] {
        db.build_index_to_ready(id, 256).unwrap();
        db.commit().unwrap();
    }

    // A small typed graph: every row points at the next one in its cluster,
    // so a two-hop traversal from `k00000` has a known answer.
    db.enable_graph().unwrap();
    db.commit().unwrap();
    let context = db.create_graph_context("routes").unwrap();
    let near = db.create_edge_type("near").unwrap();
    db.commit().unwrap();
    for i in 0..ROWS {
        let from = db.get(place, &rows[i].key).unwrap().unwrap().id;
        let to_index = (i + CLUSTERS.len()) % ROWS;
        let to = db.get(place, &rows[to_index].key).unwrap().unwrap().id;
        db.put_edge(context, from, near, to, &json!({})).unwrap();
        if (i + 1) % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    let keys = rows.iter().map(|row| row.key.clone()).collect();
    Fixture {
        path: dir.to_path_buf(),
        db,
        place,
        rows,
        keys,
        context,
        near,
        index,
    }
}

/// The centre of the first cluster, which every spatial statement in these
/// suites is written around.
pub fn centre() -> Point {
    Point::new(CLUSTERS[0].0, CLUSTERS[0].1).unwrap()
}

pub fn query_vector() -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = 0.9;
    v[1] = 0.4;
    unit(v)
}
