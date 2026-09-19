//! The E4 arm of `battle50k` must answer the questions it claims to answer.
//!
//! A two-arm benchmark is only a measurement while both arms return the same
//! rows, and the first thing to establish is that ONE arm returns the RIGHT
//! rows. This writes a 200-row synthetic corpus in the same shape as
//! `places-50000.jsonl` — a key, a two-word name, an eight-word description,
//! a yyyymmdd birth date, one of eight categories, a point, a square plot and
//! a 32-dimensional unit vector — plus a `queries.json` with the fifty
//! instances of every array the battery reads, runs the whole E4 arm over it,
//! and checks its answers against brute force over the same 200 rows:
//!
//!   * every FILTER case's `total_rows` must equal the brute-force count,
//!     summed over all fifty query instances;
//!   * `knn_10`'s `first_keys` must equal a brute-force geodesic sort of
//!     every row by distance from `points[0]`, ties broken by row order, the
//!     way `QueryOrder::Distance` breaks them.
//!
//! The brute force owns its own predicates for the scalar, point and text
//! cases. The geometry cases call `spatial_geometry` directly, which is the
//! routine `query.rs` refines a geometry posting with
//! (`geometry_predicate_matches`, src/query.rs:1526) — the index is a
//! bounding-box candidate filter in front of it, so agreement here is a
//! statement about the candidate cover being a true superset, which is the
//! part that can actually be wrong.
//!
//! The binary is included by path rather than duplicated, so the thing tested
//! is the thing a run would execute. NOTE: `cargo build --release` does not
//! build test targets, so a green release build says nothing about this file.

#[allow(dead_code)]
#[path = "../src/bin/battle50k.rs"]
mod battle50k;

use battle50k::{
    load_corpus, load_queries, run_arm, Arm, CaseKind, Corpus, Options, Queries, Row, BATTERY,
    DIM, INSTANCES, K, KINDS,
};
use e4_prototype::{
    collections::Geom,
    spatial_geometry,
    spatial_math::{wgs84_distance_metres, Point},
};
use serde_json::{json, Value};
use std::{fs, path::Path};

const ROWS: usize = 200;

/// Lowercase ASCII words only, one space between them, so this file's
/// `split_whitespace` tokenisation and E4's case-folding alphanumeric-run
/// analyzer produce the identical token set. Any word with punctuation in it
/// would make the brute force a different question.
const VOCAB: [&str; 12] = [
    "kebun", "sekolah", "jembatan", "bengkel", "desa", "kopi", "sawah", "danau", "pasar", "kantor",
    "hutan", "warung",
];

/// Exactly eight, because `load_corpus` refuses a corpus whose distinct
/// `kind` values are not eight — the `*_kind` cases index `kinds[i % 8]`.
const KIND_NAMES: [&str; KINDS] = [
    "depot", "farm", "home", "mill", "park", "port", "school", "shop",
];

/// The region the synthetic rows and every query instance live in, small
/// enough that a 2 km radius and a 0.02-degree box both see rows.
const WEST: f64 = 106.90;
const EAST: f64 = 107.10;
const SOUTH: f64 = -6.30;
const NORTH: f64 = -6.10;

/// popsim's xorshift, so the corpus is reproducible without a dependency.
fn rng(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn unit(r: u64) -> f64 {
    (r % 100_000) as f64 / 100_000.0
}

/// A square roughly 200 m on a side around `(lon, lat)`, closed.
fn plot_ring(lon: f64, lat: f64) -> Vec<Vec<[f64; 2]>> {
    let d = 0.001;
    vec![vec![
        [lon - d, lat - d],
        [lon + d, lat - d],
        [lon + d, lat + d],
        [lon - d, lat + d],
        [lon - d, lat - d],
    ]]
}

fn unit_vector(seed: u64) -> Vec<f32> {
    let mut r = rng(seed | 1);
    let mut v = Vec::with_capacity(DIM);
    for _ in 0..DIM {
        v.push(unit(r) as f32 - 0.5);
        r = rng(r);
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter().map(|x| x / norm).collect()
}

fn write_corpus(path: &Path) {
    let mut out = String::new();
    for i in 0..ROWS {
        let mut r = rng((i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let first = VOCAB[(r % 12) as usize];
        r = rng(r);
        let second = VOCAB[(r % 12) as usize];
        r = rng(r);
        let mut words = Vec::with_capacity(8);
        for _ in 0..8 {
            words.push(VOCAB[(r % 12) as usize]);
            r = rng(r);
        }
        let lon = WEST + unit(r) * (EAST - WEST);
        r = rng(r);
        let lat = SOUTH + unit(r) * (NORTH - SOUTH);
        r = rng(r);
        let year = 1940 + (r % 80) as i64;
        r = rng(r);
        let month = 1 + (r % 12) as i64;
        r = rng(r);
        let day = 1 + (r % 28) as i64;
        let row = json!({
            "key": format!("p{i:07}"),
            "name": format!("{first} {second}"),
            "desc": words.join(" "),
            "born": year * 10_000 + month * 100 + day,
            "kind": KIND_NAMES[i % KINDS],
            "loc": {"type": "Point", "coordinates": [lon, lat]},
            "plot": {"type": "Polygon", "coordinates": plot_ring(lon, lat)},
            "emb": unit_vector(i as u64 + 7),
        });
        out.push_str(&row.to_string());
        out.push('\n');
    }
    fs::write(path, out).expect("corpus is written");
}

fn write_queries(path: &Path) {
    let mut points = Vec::with_capacity(INSTANCES);
    let mut boxes = Vec::with_capacity(INSTANCES);
    let mut polygons = Vec::with_capacity(INSTANCES);
    let mut radii = Vec::with_capacity(INSTANCES);
    let mut vectors = Vec::with_capacity(INSTANCES);
    let mut terms = Vec::with_capacity(INSTANCES);
    for i in 0..INSTANCES {
        let mut r = rng((i as u64 + 101).wrapping_mul(0xA24B_AED4_963E_E407) | 1);
        let lon = WEST + unit(r) * (EAST - WEST);
        r = rng(r);
        let lat = SOUTH + unit(r) * (NORTH - SOUTH);
        points.push([lon, lat]);
        // The file's own order: [minlon, maxlon, minlat, maxlat].
        boxes.push([lon - 0.02, lon + 0.02, lat - 0.02, lat + 0.02]);
        polygons.push(json!({
            "type": "Polygon",
            "coordinates": [[
                [lon - 0.03, lat - 0.03],
                [lon + 0.03, lat - 0.02],
                [lon + 0.02, lat + 0.03],
                [lon - 0.03, lat + 0.02],
                [lon - 0.03, lat - 0.03],
            ]],
        }));
        radii.push([lon, lat, 2_000.0]);
        vectors.push(unit_vector(i as u64 + 9_001));
        terms.push(VOCAB[i % VOCAB.len()].to_string());
    }
    let queries = json!({
        "points": points,
        "boxes": boxes,
        "polygons": polygons,
        "radii": radii,
        "vectors": vectors,
        "terms": terms,
    });
    fs::write(path, serde_json::to_string(&queries).expect("queries serialise"))
        .expect("queries are written");
}

// ── the brute force ───────────────────────────────────────────────────────

fn tokens(row: &Row) -> Vec<String> {
    format!("{} {}", row.name, row.desc)
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn has_term(row: &Row, term: &str) -> bool {
    let wanted = term.to_lowercase();
    tokens(row).iter().any(|t| *t == wanted)
}

fn row_point(row: &Row) -> Point {
    Point::new(row.lon, row.lat).expect("synthetic row holds a valid point")
}

/// Does this row match this case at this query instance? One independent
/// implementation of every filter in the battery.
fn brute_matches(row: &Row, corpus: &Corpus, q: &Queries, name: &str, i: usize) -> bool {
    let kind = &corpus.kinds[i % KINDS];
    let point = Geom::Point(q.points[i][0], q.points[i][1]);
    let (born_lower, born_upper) = q.born_range(i);
    let in_radius = || {
        let centre = q.radius_centre(i).expect("radius centre is valid");
        wgs84_distance_metres(centre, row_point(row)) <= q.radius_metres(i)
    };
    let in_born = || row.born >= born_lower && row.born <= born_upper;
    match name {
        "pt_radius" => in_radius(),
        "pt_bbox" => q.bounds(i).expect("box is valid").contains(row_point(row)),
        "plot_within_box" => spatial_geometry::within(&row.plot, &q.box_polygon(i)),
        "plot_contains_pt" => spatial_geometry::contains(&row.plot, &point),
        "plot_intersects" => spatial_geometry::intersects(&row.plot, &q.polygons[i]),
        "plot_dwithin_1km" => spatial_geometry::dwithin_m(&row.plot, &point, 1_000.0),
        "plot_vs_poly_within" => spatial_geometry::within(&row.plot, &q.polygons[i]),
        "text_one" => has_term(row, &q.terms[i]),
        "text_two" => {
            has_term(row, &q.terms[i]) && has_term(row, &q.terms[(i + 1) % INSTANCES])
        }
        "text_and_kind" => has_term(row, &q.terms[i]) && row.kind == *kind,
        "born_range" => in_born(),
        "kind_eq" => row.kind == *kind,
        "radius_and_born" => in_radius() && in_born(),
        other => panic!("no brute force for filter case `{other}`"),
    }
}

fn brute_total(corpus: &Corpus, q: &Queries, name: &str) -> u64 {
    (0..INSTANCES)
        .map(|i| {
            corpus
                .rows
                .iter()
                .filter(|row| brute_matches(row, corpus, q, name, i))
                .count() as u64
        })
        .sum()
}

/// The ten nearest rows to `points[0]`, ascending geodesic distance, ties
/// broken by row order — which is entity-sequence order, which is how
/// `QueryOrder::Distance` breaks them.
fn brute_knn(corpus: &Corpus, q: &Queries) -> Vec<String> {
    let centre = q.point(0).expect("points[0] is valid");
    let mut ranked: Vec<(f64, usize)> = corpus
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| (wgs84_distance_metres(centre, row_point(row)), i))
        .collect();
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    ranked
        .iter()
        .take(K)
        .map(|(_, i)| corpus.rows[*i].key.clone())
        .collect()
}

// ── the test ──────────────────────────────────────────────────────────────

fn case_of<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["cases"]
        .as_array()
        .expect("cases is an array")
        .iter()
        .find(|case| case["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("report has no case `{name}`"))
}

#[test]
fn the_e4_arm_answers_what_brute_force_answers() {
    let dir = tempfile::tempdir().expect("temp dir");
    let data = dir.path().join("places-200.jsonl");
    let queries_path = dir.path().join("queries.json");
    let out = dir.path().join("battle50k-e4.json");
    write_corpus(&data);
    write_queries(&queries_path);

    let mut options = Options::new(Arm::E4, &data, &queries_path, &out);
    options.db_dir = dir.path().join("e4-db");
    let report = run_arm(&options).expect("the E4 arm runs");

    assert_eq!(
        report["rows"].as_u64(),
        Some(ROWS as u64),
        "the arm must have loaded every synthetic row"
    );
    assert_eq!(
        report["arm"].as_str(),
        Some("e4"),
        "the report must name its arm"
    );

    let corpus = load_corpus(&data).expect("corpus reloads");
    let queries = load_queries(&queries_path).expect("queries reload");

    for spec in &BATTERY {
        let case = case_of(&report, spec.name);
        assert_eq!(
            case["queries"].as_u64(),
            Some(INSTANCES as u64),
            "{}: every case runs {INSTANCES} query instances",
            spec.name
        );
        match spec.kind {
            CaseKind::Filter => {
                assert_eq!(
                    case["total_rows"].as_u64(),
                    Some(brute_total(&corpus, &queries, spec.name)),
                    "{}: the arm and brute force must agree on the row count",
                    spec.name
                );
                assert!(
                    case["median_us"].is_f64(),
                    "{}: a filter case must report a median",
                    spec.name
                );
            }
            CaseKind::Ranked => {
                let returned = case["first_keys"].as_array().expect("first_keys is an array");
                assert!(
                    returned.len() <= K,
                    "{}: a top-{K} case must not return more than {K} keys",
                    spec.name
                );
                assert!(
                    case["median_us"].is_f64(),
                    "{}: a ranked case must report a median",
                    spec.name
                );
            }
            CaseKind::Approx => {
                let recall = case["recall_at_k"].as_f64().unwrap_or_else(|| {
                    panic!("{}: an approximate case must report recall", spec.name)
                });
                assert!(
                    (0.0..=1.0).contains(&recall),
                    "{}: recall {recall} is outside 0..=1",
                    spec.name
                );
            }
        }
    }

    let knn = case_of(&report, "knn_10");
    let returned: Vec<String> = knn["first_keys"]
        .as_array()
        .expect("knn_10 first_keys is an array")
        .iter()
        .map(|k| k.as_str().expect("a key is a string").to_owned())
        .collect();
    assert_eq!(
        returned,
        brute_knn(&corpus, &queries),
        "knn_10 must equal a brute-force geodesic sort from points[0]"
    );

    let stages = report["stages"].as_array().expect("stages is an array");
    let disk = stages
        .iter()
        .find(|s| s["name"].as_str() == Some("disk_bytes"))
        .expect("the report carries a disk_bytes stage");
    assert!(
        disk["bytes"].as_u64().unwrap_or(0) > 0,
        "a loaded database occupies bytes on disk"
    );
    assert!(
        !report["deviations"]
            .as_array()
            .expect("deviations is an array")
            .is_empty(),
        "the arm must name what it cannot express"
    );
    assert!(
        out.is_file(),
        "the arm must write its report to --out"
    );
}
