//! Write the preserved databases of one RELEASE (`CONTRACT.md` Law 8,
//! `L8-COMPAT`), and the answers that release gave on them.
//!
//! Law 8 asks that a file a released build wrote opens in every later build
//! with the same answers, no migration and no index rebuild. The v2 corpora
//! (`docs/format-v2-fixtures`) pin the entity format; these pin what a
//! RELEASE shipped: schemas, column rules, every index family, the graph and
//! its SQL/PGQ definitions -- each written through SQL, as a user writes it.
//!
//! The program is built against the release's own source: a worktree at the
//! release tag with this one file copied into `lang/examples/`
//! (`docs/core/RELEASE_FIXTURES.md` has the commands). It touches only SQL
//! and the engine calls every release since 0.18.0 has, so the same file
//! builds against each of them.
//!
//! ```text
//! release_fixtures OUT_DIR RELEASE REVISION
//! ```
//!
//! writes `OUT_DIR/<database>/` for each database below, `EXPECTED.json`
//! beside each one's files (every query and the rows the release answered),
//! and `OUT_DIR/INDEX.json` (every file's SHA-256 and size). The test that
//! reads them is `lang/tests/release_compat.rs`; it never writes them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_core::internal::logical_features;
use sekejap_lang::{Param, SqlDatabase, SqlResult, SqlValue};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

/// Every table, rule and index family 0.18 ships, in a small travel world.
const SCHEMA: &[&str] = &[
    "CREATE SCHEMA travel",
    "CREATE TABLE place (
        _key TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        area TEXT DEFAULT 'Badung',
        rating REAL,
        visits INT NOT NULL DEFAULT 0,
        opened DATE,
        open_now BOOLEAN,
        info JSONB,
        description TEXT,
        geometry GEOMETRY(Point,4326),
        taste VECTOR(4)
    )",
    "CREATE TABLE travel.guide (_key TEXT PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE, joined TIMESTAMPTZ)",
    "CREATE TABLE travel.region (_key TEXT PRIMARY KEY, name TEXT, shape GEOMETRY(Polygon,4326))",
    "CREATE TABLE dish (_key TEXT PRIMARY KEY, name TEXT, price INT, embedding VECTOR(4), flavour VECTOR(4))",
    "CREATE TABLE guided (
        guide TEXT REFERENCES travel.guide,
        place TEXT REFERENCES place,
        score REAL,
        note TEXT DEFAULT 'none',
        PRIMARY KEY (guide, place)
    )",
    "CREATE INDEX place_rating ON place (rating)",
    "CREATE INDEX place_name_lower ON place (lower(name))",
    "CREATE INDEX place_kind ON place ((info->>'kind'))",
    "CREATE INDEX place_text ON place USING gin (to_tsvector('simple', description))",
    "CREATE INDEX place_spot ON place USING gist (geometry)",
    "CREATE INDEX place_taste ON place USING quantized (taste vector_cosine_ops)",
    "CREATE INDEX region_shape ON travel.region USING gist (shape)",
    "CREATE INDEX dish_exact ON dish USING exact (embedding)",
    // Vamana on a column no exact index covers: with both, the planner
    // answers exactly unless `ef_search` is set, and the graph would go unread.
    "CREATE INDEX dish_graph ON dish USING vamana (flavour vector_cosine_ops)",
    "ALTER PROPERTY GRAPH base ADD EDGE TABLES (
        guided SOURCE KEY (guide) REFERENCES travel.guide (_key)
               DESTINATION KEY (place) REFERENCES place (_key))",
    "CREATE PROPERTY GRAPH tours
        VERTEX TABLES (travel.guide AS guide LABEL person, place)
        EDGE TABLES (guided LABEL led)",
    "CREATE TABLE scratch (_key TEXT PRIMARY KEY, v INT)",
    "INSERT INTO scratch (_key, v) VALUES ('a', 1)",
    "DROP TABLE scratch",
];

/// The places: name, area (None = the default), rating, kind, description,
/// longitude, latitude, taste.
const PLACES: &[(&str, &str, Option<&str>, f64, &str, &str, f64, f64, [f64; 4])] = &[
    ("uluwatu", "Uluwatu Temple", None, 4.8, "temple", "cliff temple above the ocean with a sunset dance", 115.084, -8.829, [0.9, 0.1, 0.0, 0.0]),
    ("tanah-lot", "Tanah Lot", Some("Tabanan"), 4.7, "temple", "sea temple on a rock reached at low tide", 115.087, -8.621, [0.8, 0.2, 0.0, 0.0]),
    ("kuta-beach", "Kuta Beach", None, 4.3, "beach", "long sandy beach for surfing lessons and sunset walks", 115.168, -8.718, [0.1, 0.9, 0.0, 0.0]),
    ("seminyak", "Seminyak Beach", None, 4.5, "beach", "wide beach with sunset cafes and quiet mornings", 115.155, -8.691, [0.2, 0.8, 0.0, 0.0]),
    ("jimbaran", "Jimbaran Bay", None, 4.6, "beach", "calm bay with grilled seafood dinners on the sand", 115.165, -8.770, [0.1, 0.6, 0.3, 0.0]),
    ("ubud-market", "Ubud Art Market", Some("Gianyar"), 4.2, "market", "crafts, woven bags and paintings in the town centre", 115.262, -8.507, [0.0, 0.1, 0.1, 0.8]),
    ("tegallalang", "Tegallalang Rice Terrace", Some("Gianyar"), 4.6, "nature", "green rice terraces with a quiet morning walk", 115.279, -8.434, [0.3, 0.1, 0.0, 0.6]),
    ("monkey-forest", "Sacred Monkey Forest", Some("Gianyar"), 4.5, "nature", "forest temple where monkeys roam the paths", 115.259, -8.519, [0.6, 0.0, 0.0, 0.4]),
    ("batur", "Mount Batur", Some("Bangli"), 4.7, "nature", "volcano sunrise hike above the crater lake", 115.375, -8.242, [0.2, 0.0, 0.0, 0.8]),
    ("sanur", "Sanur Beach", Some("Denpasar"), 4.4, "beach", "quiet sunrise beach with a long walking path", 115.263, -8.679, [0.2, 0.7, 0.1, 0.0]),
];

const DISHES: &[(&str, &str, i64, [f64; 4])] = &[
    ("nasi-campur", "Nasi Campur", 35, [0.7, 0.2, 0.1, 0.0]),
    ("sate-lilit", "Sate Lilit", 30, [0.6, 0.3, 0.1, 0.0]),
    ("bebek-betutu", "Bebek Betutu", 90, [0.5, 0.1, 0.4, 0.0]),
    ("lawar", "Lawar", 25, [0.3, 0.5, 0.2, 0.0]),
    ("jaja-bali", "Jaja Bali", 15, [0.0, 0.1, 0.1, 0.8]),
    ("gado-gado", "Gado-Gado", 28, [0.2, 0.7, 0.1, 0.0]),
];

fn vector(v: &[f64; 4]) -> String {
    format!("[{}, {}, {}, {}]", v[0], v[1], v[2], v[3])
}

fn populate(db: &mut Database) {
    for sql in SCHEMA {
        run(db, sql);
    }
    for (i, (key, name, area, rating, kind, description, lon, lat, taste)) in PLACES.iter().enumerate() {
        // No area: the column is left out and its DEFAULT fills it.
        let (area_column, area_value) = area.map_or((String::new(), String::new()), |a| {
            (", area".to_owned(), format!(", '{a}'"))
        });
        // `info` arrives as a JSON parameter, as an application writes it.
        // Sanur's is a text literal instead: 0.18.3 stores that as a JSON
        // STRING (PostgreSQL would parse it), and the file keeps that string.
        let info = json!({"kind": kind, "tier": i % 3});
        let (info_sql, params) = if *key == "sanur" {
            (format!("'{info}'"), vec![])
        } else {
            ("$1".to_owned(), vec![Param::Json(info)])
        };
        let sql = format!(
            r#"INSERT INTO place (_key, name{area_column}, rating, opened, open_now, info, description, geometry, taste)
               VALUES ('{key}', '{name}'{area_value}, {rating}, '2020-01-{day:02}', {open}, {info_sql},
                       '{description}', '{{"type":"Point","coordinates":[{lon},{lat}]}}', '{taste}')"#,
            day = i + 1,
            open = i % 3 != 0,
            taste = vector(taste),
        );
        db.sql(&sql, &params).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    for (key, name, price, embedding) in DISHES {
        run(
            db,
            &format!(
                "INSERT INTO dish (_key, name, price, embedding, flavour) VALUES ('{key}', '{name}', {price}, '{}', '{}')",
                vector(embedding),
                vector(&[embedding[3], embedding[2], embedding[1], embedding[0]])
            ),
        );
    }
    run(
        db,
        "INSERT INTO travel.guide (_key, name, email, joined) VALUES
           ('made', 'Made', 'made@example.com', '2023-03-01T08:00:00Z'),
           ('nyoman', 'Nyoman', 'nyoman@example.com', '2023-05-12T09:30:00Z'),
           ('ketut', 'Ketut', NULL, '2024-01-20T07:15:00Z')",
    );
    run(
        db,
        r#"INSERT INTO travel.region (_key, name, shape) VALUES
           ('south', 'South coast', '{"type":"Polygon","coordinates":[[[115.05,-8.85],[115.30,-8.85],[115.30,-8.60],[115.05,-8.60],[115.05,-8.85]]]}'),
           ('highlands', 'Highlands', '{"type":"Polygon","coordinates":[[[115.20,-8.55],[115.45,-8.55],[115.45,-8.20],[115.20,-8.20],[115.20,-8.55]]]}')"#,
    );
    run(
        db,
        "INSERT INTO guided (guide, place, score) VALUES
           ('made', 'uluwatu', 4.9), ('made', 'kuta-beach', 4.1), ('made', 'jimbaran', 4.4),
           ('nyoman', 'ubud-market', 4.0), ('nyoman', 'tegallalang', 4.7), ('nyoman', 'monkey-forest', 4.3),
           ('ketut', 'batur', 4.8)",
    );
    run(db, "INSERT INTO guided (guide, place, score, note) VALUES ('ketut', 'sanur', 4.2, 'sunrise')");
    // Untyped edges in the base graph and in a graph context.
    let id = |db: &mut Database, key: &str| match run(db, &format!("SELECT _id FROM place WHERE _key = '{key}'")) {
        SqlResult::Rows { rows, .. } => match rows[0].values[0] {
            SqlValue::Id(id) => id,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    for (a, b, context) in [
        ("seminyak", "kuta-beach", ""),
        ("kuta-beach", "jimbaran", ""),
        ("jimbaran", "uluwatu", ""),
        ("ubud-market", "monkey-forest", ""),
        ("monkey-forest", "tegallalang", "routes"),
        ("tegallalang", "batur", "routes"),
    ] {
        let (a, b) = (id(db, a), id(db, b));
        db.link(a, "near", b, context, &json!({"km": 5})).expect("link");
    }
    // An update, a delete and an upsert, so the file holds more than inserts.
    run(db, "UPDATE place SET visits = 12, rating = 4.9 WHERE _key = 'uluwatu'");
    run(db, "DELETE FROM guided WHERE guide = 'made' AND place = 'kuta-beach'");
    run(db, "INSERT INTO place (_key, name, description) VALUES ('closed-kiosk', 'Closed Kiosk', 'kiosk by the road')");
    run(db, "DELETE FROM place WHERE _key = 'closed-kiosk'");
    run(
        db,
        "INSERT INTO guided (guide, place, score) VALUES ('nyoman', 'tegallalang', 4.9)
         ON CONFLICT (guide, place) DO UPDATE SET score = EXCLUDED.score",
    );
    // A text index built over rows that already exist: the late, sorted
    // build, which writes the packed segment tier.
    run(db, "CREATE TABLE review (_key TEXT PRIMARY KEY, body TEXT)");
    for i in 0..400 {
        let (a, b) = (WORDS[i % WORDS.len()], WORDS[(i * 7 + 3) % WORDS.len()]);
        run(db, &format!("INSERT INTO review (_key, body) VALUES ('r{i:03}', '{a} {b} visit number {i}')"));
    }
    run(db, "CREATE INDEX review_text ON review USING gin (to_tsvector('simple', body))");
    // A table whose shape changed while it held rows, so its rows sit under
    // three layouts: ADD then DROP COLUMN (added for the 0.18.5 fixtures; a
    // dropped name is never added back, which 0.18 would show wrongly).
    run(db, "CREATE TABLE stay (_key TEXT PRIMARY KEY, name TEXT, nights INT)");
    run(db, "INSERT INTO stay (_key, name, nights) VALUES ('s1', 'Cliff Villa', 3), ('s2', 'Rice Hut', 2), ('s3', 'Reef Bungalow', 5)");
    run(db, "ALTER TABLE stay ADD COLUMN host TEXT");
    run(db, "INSERT INTO stay (_key, name, nights, host) VALUES ('s4', 'Garden Room', 1, 'made'), ('s5', 'Lake Lodge', 4, 'ketut')");
    run(db, "UPDATE stay SET host = 'nyoman' WHERE _key = 's1'");
    run(db, "ALTER TABLE stay DROP COLUMN name");
    run(db, "INSERT INTO stay (_key, nights, host) VALUES ('s6', 6, 'wayan')");
    run(db, "COMMIT");
}

const WORDS: &[&str] = &[
    "sunset", "temple", "beach", "rice", "volcano", "market", "quiet", "surf", "dance", "forest", "reef", "sunrise",
];

/// The writes the WAL-pending database commits after its checkpoint.
const PENDING: &[&str] = &[
    "INSERT INTO place (_key, name, rating, description) VALUES ('lovina', 'Lovina Beach', 4.1, 'black sand beach with dolphins at dawn')",
    "UPDATE place SET visits = 3 WHERE _key = 'batur'",
    "DELETE FROM guided WHERE guide = 'ketut' AND place = 'sanur'",
    "INSERT INTO travel.guide (_key, name) VALUES ('wayan', 'Wayan')",
    "COMMIT",
];

/// Every question the release answers, each ordered so the answer is one
/// sequence.
const QUERIES: &[&str] = &[
    "SELECT _key, name, area, rating, visits, opened, open_now, info, description FROM place ORDER BY _key",
    "SELECT _key, name, email, joined FROM travel.guide ORDER BY _key",
    "SELECT _key, name, price FROM dish ORDER BY _key",
    "SELECT guide, place, score, note FROM guided WHERE guide = 'made' ORDER BY place",
    "SELECT guide, place, score, note FROM guided WHERE guide = 'nyoman' ORDER BY place",
    "SELECT guide, place, score, note FROM guided WHERE guide = 'ketut' ORDER BY place",
    "SELECT count(*) FROM place",
    "SELECT area, count(*) AS n FROM place GROUP BY area ORDER BY area",
    // btree, lower() and ->> expression indexes, NULL order.
    "SELECT _key FROM place WHERE rating >= 4.6 ORDER BY rating DESC, _key",
    "SELECT _key FROM place ORDER BY rating, _key",
    "SELECT _key FROM place WHERE lower(name) = 'kuta beach'",
    "SELECT _key FROM place WHERE info->>'kind' = 'temple' ORDER BY _key",
    "SELECT _key FROM place WHERE name LIKE 'S%' ORDER BY _key",
    "SELECT _key FROM place WHERE name ILIKE '%beach%' ORDER BY _key",
    "SELECT _key FROM place WHERE (rating, _key) > (4.5, 'seminyak') ORDER BY rating, _key",
    // Full text: boolean, phrase, BM25, typo-tolerant search.
    "SELECT _key FROM place WHERE to_tsvector('simple', description) @@ to_tsquery('simple', 'sunset & beach') ORDER BY _key",
    "SELECT _key FROM place WHERE to_tsvector('simple', description) @@ to_tsquery('simple', 'sunrise | temple') ORDER BY _key",
    "SELECT _key FROM place WHERE to_tsvector('simple', description) @@ to_tsquery('simple', '\"quiet morning\"') ORDER BY _key",
    "SELECT _key, bm25(description, 'quiet sunrise beach') AS score FROM place ORDER BY bm25(description, 'quiet sunrise beach') DESC LIMIT 5",
    "SELECT _key FROM place WHERE search(description, 'sunrese') ORDER BY _key",
    // Spatial: points by distance and radius, polygons.
    "SELECT _key FROM place WHERE ST_DWithin(geometry, ST_MakePoint(115.168, -8.718)::geography, 6000.0) ORDER BY _key",
    "SELECT _key FROM place ORDER BY geometry <-> ST_MakePoint(115.26, -8.51)::geography LIMIT 3",
    "SELECT _key FROM travel.region WHERE ST_Contains(shape, ST_SetSRID(ST_MakePoint(115.279, -8.434), 4326)) ORDER BY _key",
    // Vectors: quantized, exact, the Vamana graph.
    "SELECT _key FROM place ORDER BY taste <=> '[0.9, 0.1, 0.0, 0.0]' LIMIT 3",
    "SELECT _key FROM dish ORDER BY embedding <=> '[0.2, 0.7, 0.1, 0.0]' LIMIT 3",
    "SELECT _key FROM dish ORDER BY embedding <-> '[0.6, 0.3, 0.1, 0.0]' LIMIT 2",
    "SELECT _key FROM dish ORDER BY flavour <=> '[0.0, 0.1, 0.3, 0.6]' LIMIT 3",
    "SELECT _key, taste <=> '[0.9, 0.1, 0.0, 0.0]' AS d FROM place ORDER BY taste <=> '[0.9, 0.1, 0.0, 0.0]' LIMIT 3",
    "SELECT _key FROM review WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'market & beach') ORDER BY _key",
    "SELECT count(*) FROM review WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'sunset')",
    // A table read across three layouts.
    "SELECT _key, nights, host FROM stay ORDER BY _key",
    // Row identity: a stored row keeps its id.
    "SELECT _id, _key FROM place ORDER BY _key",
    // The graph: base labels, the edge table, untyped edges, a context, the
    // named graph.
    "SELECT * FROM GRAPH_TABLE (base MATCH (g:guide WHERE g._key = 'nyoman')-[v:guided]->(p:place) RETURN p._key AS place, v.score AS score) ORDER BY place",
    "SELECT * FROM GRAPH_TABLE (base MATCH (a:place WHERE a._key = 'seminyak')-[:near]->{1,3}(b:place) RETURN b._key AS place) ORDER BY place",
    "SELECT * FROM GRAPH_TABLE (routes MATCH (a:place WHERE a._key = 'monkey-forest')-[:near]->{1,2}(b:place) RETURN b._key AS place) ORDER BY place",
    "SELECT * FROM GRAPH_TABLE (tours MATCH (g:person)-[e:led]->(p:place) RETURN g._key AS guide, p._key AS place) ORDER BY guide, place",
];

pub(crate) fn cell(value: &SqlValue) -> Value {
    match value {
        SqlValue::Missing => json!({"missing": true}),
        SqlValue::Null => Value::Null,
        SqlValue::Bool(b) => json!(b),
        SqlValue::Int(i) => json!({"int": i}),
        SqlValue::Float(f) => json!({"float": f.to_bits().to_string(), "shown": f}),
        SqlValue::Text(t) => json!(t),
        SqlValue::Json(v) => json!({"json": v}),
        SqlValue::Id(id) => json!({"id": [id.collection.0, id.sequence]}),
    }
}

fn answers(db: &mut Database) -> Value {
    let mut out = Vec::new();
    for sql in QUERIES {
        let SqlResult::Rows { columns, rows } = run(db, sql) else {
            panic!("`{sql}` answered no rows")
        };
        let rows: Vec<Value> = rows
            .iter()
            .map(|r| Value::Array(r.values.iter().map(cell).collect()))
            .collect();
        assert!(!rows.is_empty(), "`{sql}` answered nothing: a fixture query must see data");
        out.push(json!({"sql": sql, "columns": columns, "rows": rows}));
    }
    Value::Array(out)
}

fn files(dir: &Path) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name == "EXPECTED.json" || !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        out.insert(
            name,
            json!({"bytes": bytes.len(), "sha256": format!("{:x}", Sha256::digest(&bytes))}),
        );
    }
    out
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_string_pretty(value).unwrap() + "\n").unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, out, release, revision] = args.as_slice() else {
        panic!("usage: release_fixtures OUT_DIR RELEASE REVISION");
    };
    let out = PathBuf::from(out);
    assert!(!out.exists(), "{} exists: a preserved corpus is never regenerated in place", out.display());
    fs::create_dir_all(&out).unwrap();
    let mut index = Vec::new();
    for name in ["checkpointed", "wal-pending"] {
        let dir = out.join(name);
        let mut db = Database::create(&dir, cfg()).expect("create");
        let features;
        populate(&mut db);
        assert!(db.checkpoint().expect("checkpoint"), "the checkpoint was deferred");
        if name == "wal-pending" {
            // A published reader keeps the next commit in the WAL: no
            // automatic checkpoint can fold it before the process ends.
            let pin = Database::open_snapshot(&dir, cfg()).expect("pin");
            for sql in PENDING {
                run(&mut db, sql);
            }
            write_json(&dir.join("EXPECTED.json"), &answers(&mut db));
            features = format!("{:#x}", logical_features(&db));
            drop(db);
            drop(pin);
        } else {
            write_json(&dir.join("EXPECTED.json"), &answers(&mut db));
            features = format!("{:#x}", logical_features(&db));
            drop(db);
        }
        let expected = fs::read(dir.join("EXPECTED.json")).unwrap();
        index.push(json!({
            "name": name,
            "checkpointed": name == "checkpointed",
            "expected_sha256": format!("{:x}", Sha256::digest(&expected)),
            "logical_features": features,
            "files": files(&dir),
        }));
    }
    write_json(
        &out.join("INDEX.json"),
        &json!({"release": release, "revision": revision, "databases": index}),
    );
}
