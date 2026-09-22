//! The unit is the type: a spatial statement sekejap ACCEPTS means the same
//! thing when it is pasted into PostGIS (`docs/core/SPATIAL_FUNCTIONS.md`,
//! "The unit is the type").
//!
//! PostGIS measures a `geometry` distance in the SRID's units -- degrees for
//! 4326 -- and a `geography` distance in metres, and `geometry` is its
//! default. sekejap measures metres only. So every distance form below is
//! accepted exactly when PostGIS would also read it as metres, and every form
//! PostGIS would read as degrees, answer on the flat plane where sekejap
//! answers on the curve, or refuse outright, is refused here by name with the
//! spelling to use instead. Each refused case is the PostGIS 3.4.3 behaviour
//! recorded in the table of that section.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn open() -> (TempDir, Database) {
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
    for statement in [
        "CREATE TABLE spot (loc GEOMETRY(Point,4326))",
        "CREATE TABLE zone (area GEOMETRY(Polygon,4326))",
        // a, then b 0.02 degrees (about 2.2 km) east, then c about 25 km east.
        r#"INSERT INTO spot (_key, loc) VALUES ('a', '{"type":"Point","coordinates":[115.17,-8.69]}')"#,
        r#"INSERT INTO spot (_key, loc) VALUES ('b', '{"type":"Point","coordinates":[115.19,-8.69]}')"#,
        r#"INSERT INTO spot (_key, loc) VALUES ('c', '{"type":"Point","coordinates":[115.40,-8.69]}')"#,
        r#"INSERT INTO zone (_key, area) VALUES ('west', '{"type":"Polygon","coordinates":[[[115.0,-9.0],[115.3,-9.0],[115.3,-8.0],[115.0,-8.0],[115.0,-9.0]]]}')"#,
        r#"INSERT INTO zone (_key, area) VALUES ('east', '{"type":"Polygon","coordinates":[[[115.35,-9.0],[115.5,-9.0],[115.5,-8.0],[115.35,-8.0],[115.35,-9.0]]]}')"#,
    ] {
        db.sql(statement, &[])
            .unwrap_or_else(|e| panic!("`{statement}` was refused: {e:?}"));
    }
    (dir, db)
}

/// The `_key`s a statement answered, in the order it answered them.
fn keys(db: &mut Database, text: &str) -> Vec<String> {
    match db.sql(text, &[]) {
        Ok(SqlResult::Rows { rows, .. }) => rows
            .into_iter()
            .map(|row| match &row.values[0] {
                SqlValue::Text(key) => key.clone(),
                other => panic!("`{text}` answered a key of {other:?}"),
            })
            .collect(),
        other => panic!("`{text}` answered {other:?}"),
    }
}

fn sorted(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys
}

fn refusal(db: &mut Database, text: &str) -> String {
    format!(
        "{}",
        db.sql(text, &[])
            .expect_err(&format!("`{text}` was accepted"))
    )
}

#[test]
fn a_distance_marked_geography_is_metres_in_every_spelling_postgis_also_reads_as_metres() {
    let (_dir, mut db) = open();
    // Each is 5 km around `a`: it takes `a` and `b`, 2.2 km apart, and not
    // `c`, 25 km away. In degrees a radius of 5,000 would take all three.
    for text in [
        "SELECT _key FROM spot WHERE ST_DWithin(loc, ST_MakePoint(115.17, -8.69)::geography, 5000)",
        "SELECT _key FROM spot WHERE ST_DWithin(loc::geography, ST_SetSRID(ST_MakePoint(115.17, -8.69), 4326), 5000)",
        "SELECT _key FROM spot WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint(115.17, -8.69), 4326), 5000, true)",
        "SELECT _key FROM spot WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint(115.17, -8.69), 4326)::geography, 5000, true)",
    ] {
        assert_eq!(sorted(keys(&mut db, text)), ["a", "b"], "{text}");
    }
    // Nearest first from `c`, both spellings of the metre distance.
    for text in [
        "SELECT _key FROM spot ORDER BY loc <-> ST_MakePoint(115.40, -8.69)::geography LIMIT 3",
        "SELECT _key FROM spot ORDER BY ST_Distance(loc, ST_MakePoint(115.40, -8.69)::geography) LIMIT 3",
    ] {
        assert_eq!(keys(&mut db, text), ["c", "b", "a"], "{text}");
    }
}

#[test]
fn a_distance_postgis_would_read_as_degrees_is_refused_with_the_spelling_to_use() {
    let (_dir, mut db) = open();
    for (text, said) in [
        (
            "SELECT _key FROM spot WHERE ST_DWithin(loc, ST_MakePoint(115.17, -8.69), 5000)",
            "ST_DWithin needs geography",
        ),
        (
            "SELECT _key FROM spot WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint(115.17, -8.69), 4326), 5000)",
            "ST_DWithin needs geography",
        ),
        // A chain is read to its END: geography then geometry is geometry.
        (
            "SELECT _key FROM spot WHERE ST_DWithin(loc, ST_MakePoint(115.17, -8.69)::geography::geometry, 5000)",
            "ST_DWithin needs geography",
        ),
        (
            "SELECT _key FROM spot ORDER BY loc <-> ST_SetSRID(ST_MakePoint(115.40, -8.69), 4326) LIMIT 3",
            "`<->` to a point needs geography",
        ),
        (
            "SELECT _key FROM spot ORDER BY ST_Distance(loc, ST_MakePoint(115.40, -8.69)) LIMIT 3",
            "ST_Distance needs geography",
        ),
    ] {
        let shown = refusal(&mut db, text);
        assert!(shown.contains(said), "`{text}` said `{shown}`");
        assert!(shown.contains("::geography"), "the refusal names the fix: `{shown}`");
    }
}

#[test]
fn use_spheroid_false_is_a_sphere_and_is_refused_as_one() {
    let (_dir, mut db) = open();
    let shown = refusal(
        &mut db,
        "SELECT _key FROM spot WHERE ST_DWithin(loc, ST_MakePoint(115.17, -8.69)::geography, 5000, false)",
    );
    assert!(shown.contains("SPHERE"), "{shown}");
}

#[test]
fn intersects_is_on_the_curve_only_when_marked_geography() {
    let (_dir, mut db) = open();
    let text = "SELECT _key FROM zone WHERE ST_Intersects(area, ST_MakeEnvelope(115.25, -8.8, 115.38, -8.6, 4326)::geography)";
    assert_eq!(sorted(keys(&mut db, text)), ["east", "west"]);
    let shown = refusal(
        &mut db,
        "SELECT _key FROM zone WHERE ST_Intersects(area, ST_MakeEnvelope(115.25, -8.8, 115.38, -8.6, 4326))",
    );
    assert!(shown.contains("ST_Intersects needs geography"), "{shown}");
}

#[test]
fn within_and_contains_are_planar_take_a_4326_shape_and_have_no_geography_form() {
    let (_dir, mut db) = open();
    assert_eq!(
        sorted(keys(
            &mut db,
            "SELECT _key FROM spot WHERE ST_Within(loc, ST_MakeEnvelope(115.0, -9.0, 115.3, -8.0, 4326))",
        )),
        ["a", "b"]
    );
    assert_eq!(
        keys(
            &mut db,
            "SELECT _key FROM zone WHERE ST_Contains(area, ST_SetSRID(ST_MakePoint(115.1, -8.5), 4326))",
        ),
        ["west"]
    );
    assert_eq!(
        keys(
            &mut db,
            r#"SELECT _key FROM zone WHERE ST_Contains(area, ST_GeomFromGeoJSON('{"type":"Point","coordinates":[115.4,-8.5]}'))"#,
        ),
        ["east"]
    );
    for (text, said) in [
        // PostGIS: function st_within(geography, geography) does not exist.
        (
            "SELECT _key FROM spot WHERE ST_Within(loc::geography, ST_MakeEnvelope(115.0, -9.0, 115.3, -8.0, 4326))",
            "ST_Within has no geography form",
        ),
        (
            "SELECT _key FROM zone WHERE ST_Contains(area, ST_SetSRID(ST_MakePoint(115.1, -8.5), 4326)::geography)",
            "ST_Contains has no geography form",
        ),
        // PostGIS: Operation on mixed SRID geometries (Polygon, 4326) != (Point, 0).
        (
            "SELECT _key FROM zone WHERE ST_Contains(area, ST_MakePoint(115.1, -8.5))",
            "has no SRID",
        ),
        (
            "SELECT _key FROM spot WHERE ST_Within(loc, ST_MakeEnvelope(115.0, -9.0, 115.3, -8.0))",
            "has no SRID",
        ),
    ] {
        let shown = refusal(&mut db, text);
        assert!(shown.contains(said), "`{text}` said `{shown}`");
    }
}
