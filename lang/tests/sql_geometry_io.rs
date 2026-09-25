//! Geometry I/O through SQL (`docs/lang/QL_CONTRACT.md` §4.4): the shapes a
//! PostGIS client writes and reads -- `ST_GeomFromWKB`, `ST_GeomFromText`,
//! hex EWKB and WKT literals, `ST_AsBinary`, `ST_AsEWKB`, `ST_AsText`,
//! `ST_AsGeoJSON`, `ST_X`, `ST_Y` -- and the `&&` bounding-box predicate.
//!
//! The expected bytes and text are PostGIS's own, from
//! `core/engine/tests/fixtures/postgis_wkb.json` (`tools/postgis_wkb_fixture.py`).
//! The `&&` answers are checked against `spatial_geometry::bbox_overlaps`,
//! which `core/engine/tests/spatial_io.rs` pins to PostGIS's `&&` on the same
//! fixture.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_core::spatial_geometry::bbox_overlaps;
use sekejap_core::spatial_io::from_wkt;
use sekejap_lang::{prepare_sql, Param, SqlDatabase, SqlResult, SqlValue};
use serde_json::Value;
use tempfile::TempDir;

fn open(statements: &[&str]) -> (TempDir, Database) {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(
        dir.path().join("db"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    for statement in statements {
        db.sql(statement, &[])
            .unwrap_or_else(|e| panic!("`{statement}` was refused: {e}"));
    }
    (dir, db)
}

fn fixture() -> Value {
    let raw = std::fs::read_to_string("../core/engine/tests/fixtures/postgis_wkb.json")
        .expect("the PostGIS WKB fixture");
    serde_json::from_str(&raw).unwrap()
}

fn rows(db: &mut Database, text: &str, params: &[Param]) -> Vec<Vec<SqlValue>> {
    match db.sql(text, params) {
        Ok(SqlResult::Rows { rows, .. }) => rows.into_iter().map(|row| row.values).collect(),
        other => panic!("`{text}` answered {other:?}"),
    }
}

fn one_text(db: &mut Database, text: &str, params: &[Param]) -> String {
    match &rows(db, text, params)[..] {
        [row] => match &row[0] {
            SqlValue::Text(value) => value.clone(),
            other => panic!("`{text}` answered {other:?}"),
        },
        other => panic!("`{text}` answered {} rows", other.len()),
    }
}

fn keys(db: &mut Database, text: &str, params: &[Param]) -> Vec<String> {
    let mut keys: Vec<String> = rows(db, text, params)
        .into_iter()
        .map(|row| match &row[0] {
            SqlValue::Text(key) => key.clone(),
            other => panic!("`{text}` answered a key of {other:?}"),
        })
        .collect();
    keys.sort();
    keys
}

fn refusal(db: &mut Database, text: &str, params: &[Param]) -> String {
    format!("{}", db.sql(text, params).expect_err(&format!("`{text}` was accepted")))
}

/// Every PostGIS case goes IN as WKB and comes OUT in every format, and each
/// output is the string PostGIS printed for the same shape.
#[test]
fn every_shape_written_as_wkb_reads_back_as_postgis_prints_it() {
    let (_dir, mut db) = open(&["CREATE TABLE shape (g GEOMETRY)"]);
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    for (at, case) in cases.iter().enumerate() {
        let key = format!("c{at:03}");
        let wkb = case["wkb_xdr"].as_str().unwrap();
        db.sql(
            "INSERT INTO shape (_key, g) VALUES ($1, ST_GeomFromWKB($2::bytea, 4326))",
            &[Param::Text(key.clone()), Param::Text(format!("\\x{wkb}"))],
        )
        .unwrap_or_else(|e| panic!("{}: {e}", case["wkt"]));
    }
    for (at, case) in cases.iter().enumerate() {
        let key = format!("c{at:03}");
        let got = &rows(
            &mut db,
            "SELECT ST_AsBinary(g, 'NDR'), ST_AsBinary(g, 'XDR'), ST_AsEWKB(g, 'NDR'), ST_AsText(g), ST_AsGeoJSON(g) FROM shape WHERE _key = $1",
            &[Param::Text(key)],
        )[0];
        let wkt = case["wkt"].as_str().unwrap();
        let text = |v: &SqlValue| match v {
            SqlValue::Text(t) => t.clone(),
            other => panic!("{wkt}: {other:?}"),
        };
        assert_eq!(text(&got[0]), format!("\\x{}", case["wkb_ndr"].as_str().unwrap()), "{wkt}");
        assert_eq!(text(&got[1]), format!("\\x{}", case["wkb_xdr"].as_str().unwrap()), "{wkt}");
        assert_eq!(text(&got[2]), format!("\\x{}", case["ewkb_ndr"].as_str().unwrap()), "{wkt}");
        assert_eq!(text(&got[3]), case["text"].as_str().unwrap(), "{wkt}");
        assert_eq!(text(&got[4]), case["geojson"].as_str().unwrap(), "{wkt}");
    }
}

#[test]
fn every_writing_form_postgis_reads_stores_the_same_point() {
    let (_dir, mut db) = open(&[
        "CREATE TABLE spot (loc GEOMETRY(Point,4326))",
        "INSERT INTO spot (_key, loc) VALUES ('wkb', ST_GeomFromWKB('\\x0101000000000000000000f03f0000000000000040', 4326))",
        "INSERT INTO spot (_key, loc) VALUES ('ewkb', ST_GeomFromEWKB('0101000020e6100000000000000000f03f0000000000000040'))",
        "INSERT INTO spot (_key, loc) VALUES ('text', ST_GeomFromText('POINT(1 2)', 4326))",
        "INSERT INTO spot (_key, loc) VALUES ('ewkt', ST_GeomFromEWKT('SRID=4326;POINT(1 2)'))",
        "INSERT INTO spot (_key, loc) VALUES ('made', ST_SetSRID(ST_MakePoint(1, 2), 4326))",
        // A bare literal reads as a `geometry` literal does in PostgreSQL:
        // hex EWKB, (E)WKT, or GeoJSON.
        "INSERT INTO spot (_key, loc) VALUES ('hex', '0101000020E6100000000000000000F03F0000000000000040')",
        "INSERT INTO spot (_key, loc) VALUES ('wkt', 'SRID=4326;POINT(1 2)')",
        r#"INSERT INTO spot (_key, loc) VALUES ('json', '{"type":"Point","coordinates":[1,2]}')"#,
    ]);
    let all = rows(&mut db, "SELECT _key, ST_AsText(loc), ST_X(loc), ST_Y(loc), ST_SRID(loc) FROM spot", &[]);
    assert_eq!(all.len(), 8);
    for row in all {
        assert_eq!(row[1], SqlValue::Text("POINT(1 2)".into()), "{row:?}");
        assert_eq!(row[2], SqlValue::Float(1.0), "{row:?}");
        assert_eq!(row[3], SqlValue::Float(2.0), "{row:?}");
        assert_eq!(row[4], SqlValue::Int(4326), "{row:?}");
    }
    // UPDATE writes through the same constructors.
    db.sql(
        "UPDATE spot SET loc = ST_GeomFromText('POINT(3 4)', 4326) WHERE _key = 'wkb'",
        &[],
    )
    .unwrap();
    assert_eq!(
        one_text(&mut db, "SELECT ST_AsEWKT(loc) FROM spot WHERE _key = 'wkb'", &[]),
        "SRID=4326;POINT(3 4)"
    );
}

/// `&&` over a geometry column: every PostGIS pair's first shape is stored,
/// and for each pair's second shape the answer is exactly the stored shapes
/// whose box meets it.
#[test]
fn overlaps_on_a_geometry_column_answers_what_postgis_answers() {
    let (_dir, mut db) = open(&["CREATE TABLE area (g GEOMETRY)"]);
    let fixture = fixture();
    let pairs = fixture["overlaps"].as_array().unwrap();
    let mut stored = Vec::new();
    for (at, pair) in pairs.iter().enumerate() {
        let key = format!("p{at:03}");
        let wkt = pair["a"].as_str().unwrap();
        db.sql(
            "INSERT INTO area (_key, g) VALUES ($1, ST_GeomFromText($2, 4326))",
            &[Param::Text(key.clone()), Param::Text(wkt.to_owned())],
        )
        .unwrap();
        stored.push((key, from_wkt(wkt).unwrap().geometry));
    }
    for (at, pair) in pairs.iter().enumerate() {
        let probe_wkt = pair["b"].as_str().unwrap();
        let probe = from_wkt(probe_wkt).unwrap().geometry;
        let got = keys(
            &mut db,
            "SELECT _key FROM area WHERE g && ST_GeomFromText($1, 4326)",
            &[Param::Text(probe_wkt.to_owned())],
        );
        let mut want: Vec<String> = stored
            .iter()
            .filter(|(_, g)| bbox_overlaps(g, &probe))
            .map(|(key, _)| key.clone())
            .collect();
        want.sort();
        assert_eq!(got, want, "&& {probe_wkt}");
        // And the pair's own PostGIS answer is among them, or not.
        let key = format!("p{at:03}");
        assert_eq!(
            got.contains(&key),
            pair["overlaps"].as_bool().unwrap(),
            "{} && {probe_wkt}",
            pair["a"]
        );
    }
}

/// `&&` over a POINT column is answered from the point index, whose bounds
/// are doubles; the edge is widened so the answer is still PostGIS's float4
/// one, on every side of the box and on both sides of zero.
#[test]
fn overlaps_on_a_point_column_keeps_the_float4_edge() {
    let (_dir, mut db) = open(&["CREATE TABLE spot (loc GEOMETRY(Point,4326))"]);
    let mut stored = Vec::new();
    let offsets = [0.0, 1e-10, 1e-8, 5e-8, 1e-7, 2e-7, 1e-3];
    let mut n = 0;
    for edge in [-1.0f64, 0.0, 1.0] {
        for offset in offsets {
            for (x, y) in [(edge + offset, 0.5), (edge - offset, 0.5), (0.5, edge + offset), (0.5, edge - offset)] {
                let key = format!("s{n:03}");
                n += 1;
                db.sql(
                    "INSERT INTO spot (_key, loc) VALUES ($1, ST_SetSRID(ST_MakePoint($2, $3), 4326))",
                    &[Param::Text(key.clone()), Param::Float(x), Param::Float(y)],
                )
                .unwrap();
                stored.push((key, sekejap_core::collections::Geom::Point(x, y)));
            }
        }
    }
    for (w, s, e, north) in [(0.0, 0.0, 1.0, 1.0), (-1.0, -1.0, 0.0, 0.0), (-1.0, 0.0, 1.0, 1.0)] {
        let envelope = from_wkt(&format!(
            "POLYGON(({w} {s},{e} {s},{e} {north},{w} {north},{w} {s}))"
        ))
        .unwrap()
        .geometry;
        let got = keys(
            &mut db,
            "SELECT _key FROM spot WHERE loc && ST_MakeEnvelope($1, $2, $3, $4, 4326)",
            &[Param::Float(w), Param::Float(s), Param::Float(e), Param::Float(north)],
        );
        let mut want: Vec<String> = stored
            .iter()
            .filter(|(_, g)| bbox_overlaps(g, &envelope))
            .map(|(key, _)| key.clone())
            .collect();
        want.sort();
        assert_eq!(got, want, "loc && ST_MakeEnvelope({w}, {s}, {e}, {north}, 4326)");
    }
}

#[test]
fn a_prepared_overlap_rebinds_its_envelope() {
    let (_dir, mut db) = open(&[
        "CREATE TABLE spot (loc GEOMETRY(Point,4326))",
        "INSERT INTO spot (_key, loc) VALUES ('a', ST_GeomFromText('POINT(1 1)', 4326))",
        "INSERT INTO spot (_key, loc) VALUES ('b', ST_GeomFromText('POINT(5 5)', 4326))",
    ]);
    let sql = "SELECT _key FROM spot WHERE loc && ST_MakeEnvelope($1, $2, $3, $4, 4326)";
    let first = [Param::Float(0.0), Param::Float(0.0), Param::Float(2.0), Param::Float(2.0)];
    let second = [Param::Float(4.0), Param::Float(4.0), Param::Float(6.0), Param::Float(6.0)];
    let mut prepared = prepare_sql(&db, sql, &first).unwrap();
    assert!(prepared.rebindable(), "{:?}", prepared.rebind_refusal());
    prepared.bind(&db, &second).unwrap();
    match prepared.run(&db).unwrap() {
        SqlResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[0], SqlValue::Text("b".into()));
        }
        other => panic!("{other:?}"),
    }
    let _ = keys(&mut db, sql, &first);
}

#[test]
fn a_wkb_column_is_described_as_bytea_before_any_row() {
    let (_dir, db) = open(&["CREATE TABLE shape (g GEOMETRY)"]);
    let prepared = prepare_sql(
        &db,
        "SELECT ST_AsBinary(g) AS g, ST_AsText(g), ST_X(g), _key FROM shape",
        &[],
    )
    .unwrap();
    assert_eq!(prepared.column_type(0), Some("BYTEA"), "an alias does not hide the type");
    assert_eq!(prepared.column_type(1), Some("TEXT"));
    assert_eq!(prepared.column_type(2), Some("FLOAT8"));
    assert_eq!(prepared.column_type(3), None);
}

#[test]
fn postgis_version_is_answered_and_says_what_is_not_built() {
    let (_dir, mut db) = open(&[]);
    let version = one_text(&mut db, "SELECT postgis_version()", &[]);
    assert!(version.starts_with("3.4 "), "{version}");
    assert!(version.contains("USE_GEOS=0"), "{version}");
    assert!(version.contains("USE_PROJ=0"), "{version}");
}

#[test]
fn what_postgis_would_refuse_is_refused() {
    let (_dir, mut db) = open(&[
        "CREATE TABLE spot (name TEXT, loc GEOMETRY(Point,4326))",
        "CREATE TABLE zone (area GEOMETRY(Polygon,4326))",
        "INSERT INTO zone (_key, area) VALUES ('z', ST_GeomFromText('POLYGON((0 0,1 0,1 1,0 1,0 0))', 4326))",
    ]);
    // A shape of another SRID: storage is WGS84.
    let error = refusal(&mut db, "SELECT _key FROM spot WHERE loc && ST_GeomFromText('POINT(1 2)', 3857)", &[]);
    assert!(error.contains("WGS84"), "{error}");
    let error = refusal(
        &mut db,
        "SELECT _key FROM spot WHERE loc && ST_GeomFromEWKB('0101000020110f0000000000000000f03f0000000000000040')",
        &[],
    );
    assert!(error.contains("SRID 3857"), "{error}");
    // An SRID 0 shape against a 4326 column, in a predicate and in a write.
    let error = refusal(&mut db, "SELECT _key FROM spot WHERE loc && ST_GeomFromText('POINT(1 2)')", &[]);
    assert!(error.contains("no SRID"), "{error}");
    let error = refusal(&mut db, "INSERT INTO spot (_key, loc) VALUES ('x', ST_MakePoint(1, 2))", &[]);
    assert!(error.contains("no SRID"), "{error}");
    // A Z coordinate and an empty geometry have no form here.
    let error = refusal(&mut db, "INSERT INTO spot (_key, loc) VALUES ('x', ST_GeomFromText('POINT Z (1 2 3)', 4326))", &[]);
    assert!(error.contains("two-dimensional"), "{error}");
    let error = refusal(&mut db, "INSERT INTO spot (_key, loc) VALUES ('x', ST_GeomFromText('POINT EMPTY', 4326))", &[]);
    assert!(error.contains("EMPTY"), "{error}");
    // ST_X of a polygon is an error in PostGIS too.
    let error = refusal(&mut db, "SELECT ST_X(area) FROM zone", &[]);
    assert!(error.contains("Point"), "{error}");
    // `&&` over a text column is array overlap, which has no atomic.
    let error = refusal(&mut db, "SELECT _key FROM spot WHERE name && 'x'", &[]);
    assert!(error.contains("`&&`"), "{error}");
    // A geometry output function over a column that is not a geometry.
    let error = refusal(&mut db, "SELECT ST_AsText(name) FROM spot", &[]);
    assert!(error.contains("not a geometry"), "{error}");
}
