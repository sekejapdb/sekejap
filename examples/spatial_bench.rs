// Spatial disk-first benchmark harness.
//
//   # build a DB from a GeoJSON FeatureCollection (ingest + compact)
//   cargo run --release --example spatial_bench -- build <geojson> <dbdir> [limit]
//
//   # serve it and measure query latency (RSS via `/usr/bin/time -l` wrapping this)
//   /usr/bin/time -l cargo run --release --example spatial_bench -- serve <dbdir> <heap|paged>
//
// Split build/serve so a fresh serve process's peak RSS reflects SERVE-time
// residency (the spatial grid), not the ingest peak.
use std::time::Instant;
use sekejap::CoreDB;

/// Current resident set (MB) via `ps` on self — isolates post-open (grid-build)
/// residency from query-time mmap reads. (macOS RSS conflates anon+file; the
/// clean anon/file split comes from /proc on the Pi.)
fn rss_mb() -> f64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid]).output().ok();
    out.and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0).unwrap_or(0.0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("build") => build(&args),
        Some("serve") => serve(&args),
        _ => eprintln!("usage: spatial_bench build <geojson> <dbdir> [limit] | serve <dbdir> <heap|paged>"),
    }
}

fn build(args: &[String]) {
    let geojson = &args[2];
    let dbdir = &args[3];
    let limit: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);

    let _ = std::fs::remove_dir_all(dbdir);
    std::fs::create_dir_all(dbdir).unwrap();

    let t = Instant::now();
    let bytes = std::fs::read(geojson).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let feats = v["features"].as_array().expect("FeatureCollection.features");
    println!("read {} features in {:?}", feats.len(), t.elapsed());

    let mut db = CoreDB::open(dbdir).unwrap();
    let t = Instant::now();
    let mut n = 0usize;
    for (i, f) in feats.iter().enumerate().take(limit) {
        let geom = &f["geometry"];
        if geom.is_null() { continue; }
        let props = &f["properties"];
        let name = props.get("ADM3_EN").or_else(|| props.get("ADM4_EN"))
            .or_else(|| props.get("name")).and_then(|x| x.as_str()).unwrap_or("");
        let key = format!("d{i}");
        let payload = serde_json::json!({
            "_collection": "area", "_key": key, "name": name, "geometry": geom,
        });
        db.put(&format!("area/{key}"), &payload.to_string()).unwrap();
        n += 1;
        if n % 10000 == 0 { println!("  inserted {n}"); }
    }
    println!("inserted {n} nodes in {:?}", t.elapsed());

    let t = Instant::now();
    db.compact().unwrap();
    println!("compact in {:?}", t.elapsed());
    println!("nodes={} on disk", db.node_count());
}

fn serve(args: &[String]) {
    let dbdir = &args[2];
    let mode = args.get(3).map(|s| s.as_str()).unwrap_or("heap");

    let t = Instant::now();
    let db = match mode {
        "paged" => CoreDB::open_paged(dbdir).unwrap(),
        _ => CoreDB::open(dbdir).unwrap(),
    };
    println!("[{mode}] open in {:?}, nodes(resident)={}, RSS(post-open)={:.1} MB",
             t.elapsed(), db.node_count(), rss_mb());

    // A few radius queries around Indonesian cities (lon lat), several radii.
    let points = [
        ("Jakarta", 106.8272, -6.1751),
        ("Surabaya", 112.7521, -7.2575),
        ("Denpasar", 115.2126, -8.6705),
        ("Medan", 98.6722, 3.5952),
    ];
    let radii = [1_000.0, 5_000.0, 25_000.0, 100_000.0];

    for (name, lon, lat) in points {
        for r in radii {
            let sql = format!(
                "SELECT _key FROM area WHERE ST_DWithin(geometry, POINT({lon} {lat}), {r})"
            );
            let t = Instant::now();
            let hits = db.query(&sql).map(|s| s.collect().len()).unwrap_or(0);
            let us = t.elapsed().as_micros();
            println!("[{mode}] {name:<9} r={r:>9.0}m  hits={hits:<6} {us:>8} us");
        }
    }
    println!("[{mode}] RSS(post-query)={:.1} MB", rss_mb());
}
