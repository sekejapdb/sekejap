//! Spatial queries: a GEO column + spatial index + ST_DWithin radius search.
//!
//!   cargo run --example spatial

use sekejap::CoreDB;

fn main() {
    let mut db = CoreDB::new();

    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, geometry GEO)").unwrap();
    db.execute("CREATE INDEX ON places USING spatial (geometry)").unwrap();

    // GEO values are GeoJSON. Insert via the atomic put with a JSON payload.
    for (key, name, lon, lat) in [
        ("uluwatu", "Uluwatu Temple", 115.087, -8.829),
        ("kuta", "Kuta Beach", 115.168, -8.720),
        ("ubud", "Ubud Center", 115.263, -8.507),
    ] {
        let payload = format!(
            r#"{{"_collection":"places","_key":"{key}","name":"{name}",
                 "geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#
        );
        db.put(&format!("places/{key}"), &payload).unwrap();
    }
    db.build_spatial_index();

    // Everything within 20 km of Uluwatu (lon lat).
    println!("within 20km of Uluwatu:");
    for hit in db
        .query("SELECT name FROM places WHERE ST_DWithin(geometry, POINT(115.087 -8.829), 20.0)")
        .unwrap()
        .collect()
    {
        println!("  {}", serde_json::to_string(&hit.payload.unwrap()).unwrap());
    }
}
