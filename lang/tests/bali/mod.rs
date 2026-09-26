//! The Bali fixture the GQL host-form tests share (`gql_host.rs`,
//! `gql_complete_or_error.rs`): beaches with a text, a point, a vector and a
//! rating, the towns near them, and a route between beaches. Invented data
//! from the README's tourism world.
#![allow(dead_code)]

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, GraphContextId};
use sekejap_lang::{prepare_sql, Param, SqlDatabase, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

pub fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// `(key, about, lon, lat, emb, rating)` for each beach.
pub const BEACHES: &[(&str, &str, f64, f64, [f32; 3], i64)] = &[
    ("kuta", "surf sunset nasi goreng", 115.1686, -8.7180, [1.0, 0.0, 0.0], 4),
    ("seminyak", "sunset surf shopping", 115.1580, -8.6913, [0.8, 0.6, 0.0], 5),
    ("sanur", "sunrise calm reef", 115.2626, -8.6783, [0.0, 1.0, 0.0], 4),
    ("nusa-dua", "calm reef snorkel", 115.2317, -8.8003, [0.0, 0.6, 0.8], 5),
    ("amed", "reef snorkel volcano view", 115.6600, -8.3470, [0.0, 0.0, 1.0], 3),
];

/// ```text
/// town denpasar -near-> kuta, seminyak, sanur, nusa-dua
/// town amlapura -near-> amed
/// beach kuta -route-> seminyak -route-> sanur -route-> nusa-dua -route-> amed
/// beach kuta -route-> sanur
/// ```
pub fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("host.sekejap"), cfg()).unwrap();
    for ddl in [
        "CREATE TABLE beach (about TEXT, loc GEOMETRY(Point,4326), emb VECTOR(3) NOT NULL, rating INT) WITH (index: none)",
        "CREATE TABLE town (name TEXT, loc GEOMETRY(Point,4326)) WITH (index: none)",
    ] {
        db.sql(ddl, &[]).unwrap();
    }
    let point = |lon: f64, lat: f64| Param::Text(format!(r#"{{"type":"Point","coordinates":[{lon:?},{lat:?}]}}"#));
    for (key, about, lon, lat, emb, rating) in BEACHES {
        db.sql(
            "INSERT INTO beach (_key, about, loc, emb, rating) VALUES ($1, $2, $3, $4, $5)",
            &[
                Param::Text((*key).into()),
                Param::Text((*about).into()),
                point(*lon, *lat),
                Param::Vector(emb.to_vec()),
                Param::Int(*rating),
            ],
        )
        .unwrap();
    }
    for (town, lon, lat) in [("denpasar", 115.2167, -8.6500), ("amlapura", 115.6139, -8.4500)] {
        db.sql(
            "INSERT INTO town (_key, name, loc) VALUES ($1, $1, $2)",
            &[Param::Text(town.into()), point(lon, lat)],
        )
        .unwrap();
    }
    db.sql("COMMIT", &[]).unwrap();
    for ddl in [
        "CREATE INDEX beach_about ON beach USING gin (to_tsvector('simple', about))",
        "CREATE INDEX beach_loc ON beach USING gist (loc)",
        "CREATE INDEX beach_rating ON beach USING btree (rating)",
        "CREATE INDEX beach_emb ON beach USING exact (emb)",
        "CREATE INDEX beach_emb_quantized ON beach USING quantized (emb)",
    ] {
        db.sql(ddl, &[]).unwrap();
    }
    let beach = db.collection("beach").unwrap().unwrap();
    let town = db.collection("town").unwrap().unwrap();
    let id = |db: &Database, c, key: &str| db.get(c, key).unwrap().unwrap().id;
    db.enable_graph().unwrap();
    let near = db.create_edge_type("near").unwrap();
    for (from, to) in [
        ("denpasar", "kuta"),
        ("denpasar", "seminyak"),
        ("denpasar", "sanur"),
        ("denpasar", "nusa-dua"),
        ("amlapura", "amed"),
    ] {
        let (from, to) = (id(&db, town, from), id(&db, beach, to));
        db.create_edge(GraphContextId::BASE, from, near, to, &json!({})).unwrap();
    }
    let route = db.create_edge_type("route").unwrap();
    for (from, to) in [
        ("kuta", "seminyak"),
        ("seminyak", "sanur"),
        ("sanur", "nusa-dua"),
        ("nusa-dua", "amed"),
        ("kuta", "sanur"),
    ] {
        let (from, to) = (id(&db, beach, from), id(&db, beach, to));
        db.create_edge(GraphContextId::BASE, from, route, to, &json!({})).unwrap();
    }
    db.commit().unwrap();
    db
}

pub fn run(db: &Database, body: &str, params: &[Param]) -> Result<Vec<Vec<SqlValue>>, SqlError> {
    let text = format!("SELECT * FROM GRAPH_TABLE (base {body})");
    match prepare_sql(db, &text, params)?.run(db)? {
        SqlResult::Rows { rows, .. } => Ok(rows.into_iter().map(|row| row.values).collect()),
        other => panic!("`{text}` answered {other:?}"),
    }
}

pub fn keys(rows: &[Vec<SqlValue>]) -> Vec<String> {
    rows.iter()
        .map(|row| match &row[0] {
            SqlValue::Text(key) => key.clone(),
            other => panic!("a key, not {other:?}"),
        })
        .collect()
}

pub fn float(value: &SqlValue) -> f64 {
    match value {
        SqlValue::Float(f) => *f,
        other => panic!("a double precision, not {other:?}"),
    }
}

pub fn sqlstate(error: &SqlError) -> Option<&'static str> {
    match error {
        SqlError::Coded { sqlstate, .. } => Some(sqlstate),
        _ => None,
    }
}

