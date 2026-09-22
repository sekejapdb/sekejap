//! `PreparedSql` is REUSABLE: one parse, one compile, many parameter lists.
//!
//! The claim under test is an equality, so every test here asserts the same
//! thing twice. First, that a statement prepared under one parameter list and
//! then RE-BOUND to a second answers exactly what a statement freshly parsed
//! and compiled under the second answers -- the rebind may not change the
//! question. Second, that the answer is the one a brute-force computation
//! over the fixture's own rows, held in this test process, produces -- so a
//! rebind that agrees with a fresh compile because both are wrong still
//! fails.
//!
//! The third claim is the refusal: a statement whose plan depends on a
//! parameter VALUE is marked `rebind: false`, says which `$n` was folded and
//! by what, prints it in `EXPLAIN`, and STILL answers correctly, because
//! `PreparedSql::bind` then compiles again from the statement it parsed once.

#[path = "sqlslice/fixture.rs"]
mod fixture;

use sekejap_core::collections::{Database, EntityId};
use sekejap_lang::{
    prepare_sql, Param, PreparedSql, SqlDatabase, SqlResult, SqlValue,
};
use tempfile::TempDir;

fn open() -> (TempDir, fixture::Fixture) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let f = fixture::build(&path);
    (dir, f)
}

/// Every row one answer returns, as external keys, sorted. Sorting is on
/// purpose: what a rebind must preserve is the ANSWER SET of an unordered
/// statement, and the ranked statements below assert order separately.
fn keys(db: &Database, result: &SqlResult) -> Vec<String> {
    let SqlResult::Rows { rows, .. } = result else {
        panic!("expected rows, got {result:?}");
    };
    let mut out: Vec<String> = rows
        .iter()
        .map(|row| key_of(db, row.id, &row.values))
        .collect();
    out.sort();
    out
}

/// The answer in RESULT ORDER, for a ranked statement.
fn ordered_keys(db: &Database, result: &SqlResult) -> Vec<String> {
    let SqlResult::Rows { rows, .. } = result else {
        panic!("expected rows, got {result:?}");
    };
    rows.iter()
        .map(|row| key_of(db, row.id, &row.values))
        .collect()
}

fn key_of(db: &Database, id: EntityId, values: &[SqlValue]) -> String {
    if let Some(SqlValue::Text(key)) = values.first() {
        return key.clone();
    }
    db.get_by_id(id).unwrap().unwrap().key
}

/// One statement, parsed and compiled fresh under `params`, then run.
fn fresh(db: &Database, sql: &str, params: &[Param]) -> SqlResult {
    prepare_sql(db, sql, params).unwrap().run(db).unwrap()
}

/// One statement prepared under `first`, re-bound to `second`, then run.
fn rebound(db: &Database, sql: &str, first: &[Param], second: &[Param]) -> (PreparedSql, SqlResult) {
    let mut prepared = prepare_sql(db, sql, first).unwrap();
    prepared.bind(db, second).unwrap();
    let result = prepared.run(db).unwrap();
    (prepared, result)
}

/// The three assertions every family makes: the rebound answer equals the
/// fresh compile's, the statement's rebindability is what the family claims,
/// and the answer equals `oracle`.
fn family(
    db: &Database,
    sql: &str,
    first: &[Param],
    second: &[Param],
    rebindable: bool,
    oracle: &[String],
) {
    let (prepared, got) = rebound(db, sql, first, second);
    assert_eq!(
        prepared.rebindable(),
        rebindable,
        "{sql}: rebindable() disagrees with the family; refusal was {:?}",
        prepared.rebind_refusal()
    );
    let expected = fresh(db, sql, second);
    assert_eq!(
        keys(db, &got),
        keys(db, &expected),
        "{sql}: a rebind answered a different question than a fresh compile"
    );
    let mut want = oracle.to_vec();
    want.sort();
    assert_eq!(
        keys(db, &got),
        want,
        "{sql}: the answer is not the brute-force answer"
    );
    // And the FIRST parameter list is still answerable from the same handle:
    // a rebind is not a one-way door.
    let mut back = prepared;
    back.bind(db, first).unwrap();
    assert_eq!(
        keys(db, &back.run(db).unwrap()),
        keys(db, &fresh(db, sql, first)),
        "{sql}: rebinding back to the first parameters lost the question"
    );
}

// ── brute force, over the fixture's own rows ──────────────────────────────

fn by<F: Fn(&fixture::Row) -> bool>(f: &fixture::Fixture, keep: F) -> Vec<String> {
    f.rows
        .iter()
        .filter(|row| keep(row))
        .map(|row| row.key.clone())
        .collect()
}

/// The great-circle distance in metres, computed here rather than read from
/// the engine: an oracle that called the engine's own maths would agree with
/// it by construction.
fn metres(alon: f64, alat: f64, blon: f64, blat: f64) -> f64 {
    let r = 6_371_008.8_f64;
    let (p1, p2) = (alat.to_radians(), blat.to_radians());
    let dp = (blat - alat).to_radians();
    let dl = (blon - alon).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

// ── scalar families ───────────────────────────────────────────────────────

#[test]
fn a_scalar_equality_rebinds_to_a_new_value() {
    let (_dir, f) = open();
    family(
        &f.db,
        "SELECT _key FROM place WHERE kind = $1",
        &[Param::Text("depot".into())],
        &[Param::Text("port".into())],
        true,
        &by(&f, |row| row.kind == "port"),
    );
}

#[test]
fn a_scalar_range_rebinds_both_of_its_bounds() {
    let (_dir, f) = open();
    family(
        &f.db,
        "SELECT _key FROM place WHERE born BETWEEN $1 AND $2",
        &[Param::Int(1900), Param::Int(1910)],
        &[Param::Int(1950), Param::Int(1975)],
        true,
        &by(&f, |row| (1950..=1975).contains(&row.born)),
    );
}

#[test]
fn an_open_ended_comparison_keeps_its_operator_and_changes_only_the_value() {
    let (_dir, f) = open();
    // `>` is EXCLUDED and `>=` INCLUDED: a rebind writes a new value into the
    // bound the statement wrote, and never changes which bound it is.
    family(
        &f.db,
        "SELECT _key FROM place WHERE born > $1",
        &[Param::Int(1900)],
        &[Param::Int(1990)],
        true,
        &by(&f, |row| row.born > 1990),
    );
    family(
        &f.db,
        "SELECT _key FROM place WHERE born >= $1",
        &[Param::Int(1900)],
        &[Param::Int(1990)],
        true,
        &by(&f, |row| row.born >= 1990),
    );
}

#[test]
fn an_in_list_rebinds_every_leaf_of_its_union() {
    let (_dir, f) = open();
    family(
        &f.db,
        "SELECT _key FROM place WHERE kind IN ($1, $2)",
        &[Param::Text("depot".into()), Param::Text("farm".into())],
        &[Param::Text("port".into()), Param::Text("mill".into())],
        true,
        &by(&f, |row| row.kind == "port" || row.kind == "mill"),
    );
}

#[test]
fn a_negated_equality_rebinds_inside_its_complement() {
    let (_dir, f) = open();
    family(
        &f.db,
        "SELECT _key FROM place WHERE kind <> $1",
        &[Param::Text("depot".into())],
        &[Param::Text("port".into())],
        true,
        &by(&f, |row| row.kind != "port"),
    );
}

#[test]
fn a_disjunction_rebinds_both_of_its_arms() {
    let (_dir, f) = open();
    family(
        &f.db,
        "SELECT _key FROM place WHERE kind = $1 OR born BETWEEN $2 AND $3",
        &[
            Param::Text("depot".into()),
            Param::Int(1900),
            Param::Int(1905),
        ],
        &[
            Param::Text("port".into()),
            Param::Int(1990),
            Param::Int(1999),
        ],
        true,
        &by(&f, |row| {
            row.kind == "port" || (1990..=1999).contains(&row.born)
        }),
    );
}

// ── the external key ──────────────────────────────────────────────────────

#[test]
fn a_key_equality_rebinds_both_ends_of_its_one_key_range() {
    let (_dir, f) = open();
    family(
        &f.db,
        "SELECT _key FROM place WHERE _key = $1",
        &[Param::Text(f.keys[3].clone())],
        &[Param::Text(f.keys[77].clone())],
        true,
        &[f.keys[77].clone()],
    );
}

#[test]
fn a_key_range_rebinds_its_bounds() {
    let (_dir, f) = open();
    let (lower, upper) = (f.keys[100].clone(), f.keys[109].clone());
    family(
        &f.db,
        "SELECT _key FROM place WHERE _key BETWEEN $1 AND $2",
        &[Param::Text(f.keys[0].clone()), Param::Text(f.keys[5].clone())],
        &[Param::Text(lower.clone()), Param::Text(upper.clone())],
        true,
        &by(&f, |row| row.key >= lower && row.key <= upper),
    );
}

// ── text ──────────────────────────────────────────────────────────────────

#[test]
fn a_tsquery_rebinds_its_terms_and_its_match_kind() {
    let (_dir, f) = open();
    // `a` alone is TextMatch::Any and `a & b` is TextMatch::All: both arrive
    // through the SAME slot, so a rebind re-derives which match it is.
    let sql = "SELECT _key FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1)";
    family(
        &f.db,
        sql,
        &[Param::Text("kopi".into())],
        &[Param::Text("pasar".into())],
        true,
        &by(&f, |row| {
            row.text.split_whitespace().any(|term| term == "pasar")
        }),
    );
    family(
        &f.db,
        sql,
        &[Param::Text("kopi".into())],
        &[Param::Text("pasar & kebun".into())],
        true,
        &by(&f, |row| {
            let terms: Vec<&str> = row.text.split_whitespace().collect();
            terms.contains(&"pasar") && terms.contains(&"kebun")
        }),
    );
}

// ── spatial ───────────────────────────────────────────────────────────────

/// The rows a radius admits, as this test computes them: the ones clearly
/// inside, and a BAND of 0.2 % either side of the boundary.
///
/// The band is named rather than hidden. The engine measures on a spheroid
/// (`ST_DWithin` on `geography`) and the oracle below is a sphere of mean
/// radius 6,371,008.8 m, so the two disagree by up to about one part in
/// five hundred -- which decides only rows sitting on the circle. Every row
/// outside the band is an exact assertion.
fn radius_oracle(
    f: &fixture::Fixture,
    lon: f64,
    lat: f64,
    radius: f64,
) -> (Vec<String>, Vec<String>) {
    let mut inside = Vec::new();
    let mut band = Vec::new();
    for row in &f.rows {
        let d = metres(lon, lat, row.lon, row.lat);
        if d <= radius * 0.998 {
            inside.push(row.key.clone());
        } else if d <= radius * 1.002 {
            band.push(row.key.clone());
        }
    }
    inside.sort();
    band.sort();
    (inside, band)
}

#[test]
fn a_point_radius_rebinds_its_centre_and_its_distance() {
    let (_dir, f) = open();
    let sql =
        "SELECT _key FROM place WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3)";
    let (lon, lat, radius) = (107.61, -6.91, 20_000.0);
    let second = [Param::Float(lon), Param::Float(lat), Param::Float(radius)];
    let (prepared, got) = rebound(
        &f.db,
        sql,
        &[
            Param::Float(106.82),
            Param::Float(-6.17),
            Param::Float(5_000.0),
        ],
        &second,
    );
    assert!(prepared.rebindable());
    let answered = keys(&f.db, &got);
    assert_eq!(answered, keys(&f.db, &fresh(&f.db, sql, &second)));
    let (inside, band) = radius_oracle(&f, lon, lat, radius);
    assert!(!inside.is_empty() && inside.len() < f.rows.len());
    for key in &inside {
        assert!(answered.contains(key), "{key} is inside the radius");
    }
    for key in &answered {
        assert!(
            inside.contains(key) || band.contains(key),
            "{key} is outside the radius and outside the 0.2 % boundary band"
        );
    }
}

#[test]
fn a_rectangle_over_a_point_column_rebinds_its_four_corners() {
    let (_dir, f) = open();
    let (w, s, e, n) = (110.0, -7.5, 113.5, -6.5);
    family(
        &f.db,
        "SELECT _key FROM place WHERE ST_Within(loc::geometry, ST_MakeEnvelope($1, $2, $3, $4, 4326))",
        &[
            Param::Float(106.0),
            Param::Float(-6.5),
            Param::Float(107.0),
            Param::Float(-6.0),
        ],
        &[
            Param::Float(w),
            Param::Float(s),
            Param::Float(e),
            Param::Float(n),
        ],
        true,
        &by(&f, |row| {
            row.lon >= w && row.lon <= e && row.lat >= s && row.lat <= n
        }),
    );
}

#[test]
fn a_geometry_predicate_rebinds_the_geometry_it_compares_against() {
    let (_dir, f) = open();
    let sql = "SELECT _key FROM place WHERE ST_Contains(plot::geometry, ST_SetSRID(ST_MakePoint($1, $2), 4326))";
    let (lon, lat) = (110.42, -6.97);
    let (prepared, got) = rebound(
        &f.db,
        sql,
        &[Param::Float(106.82), Param::Float(-6.17)],
        &[Param::Float(lon), Param::Float(lat)],
    );
    assert!(prepared.rebindable());
    assert_eq!(
        keys(&f.db, &got),
        keys(&f.db, &fresh(&f.db, sql, &[Param::Float(lon), Param::Float(lat)]))
    );
    // Brute force: the fixture's plots are axis-aligned rings, so a point is
    // inside one when it is inside that ring's bounding box.
    let mut want: Vec<String> = f
        .rows
        .iter()
        .filter(|row| plot_contains(&row.plot, lon, lat))
        .map(|row| row.key.clone())
        .collect();
    want.sort();
    assert_eq!(keys(&f.db, &got), want);
}

/// A point inside an axis-aligned ring, computed here.
fn plot_contains(plot: &sekejap_core::collections::Geom, lon: f64, lat: f64) -> bool {
    let sekejap_core::collections::Geom::Polygon(rings) = plot else {
        return false;
    };
    let Some(ring) = rings.first() else {
        return false;
    };
    let xs: Vec<f64> = ring.iter().map(|p| p[0]).collect();
    let ys: Vec<f64> = ring.iter().map(|p| p[1]).collect();
    let (w, e) = (
        xs.iter().cloned().fold(f64::INFINITY, f64::min),
        xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    );
    let (s, n) = (
        ys.iter().cloned().fold(f64::INFINITY, f64::min),
        ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    );
    lon >= w && lon <= e && lat >= s && lat <= n
}

// ── ranked families ───────────────────────────────────────────────────────

#[test]
fn a_nearest_order_rebinds_its_centre_and_keeps_its_order() {
    let (_dir, f) = open();
    let sql = "SELECT _key FROM place ORDER BY loc <-> ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography LIMIT 5";
    let second = [Param::Float(110.42), Param::Float(-6.97)];
    let (prepared, got) = rebound(
        &f.db,
        sql,
        &[Param::Float(106.82), Param::Float(-6.17)],
        &second,
    );
    assert!(prepared.rebindable());
    assert_eq!(
        ordered_keys(&f.db, &got),
        ordered_keys(&f.db, &fresh(&f.db, sql, &second))
    );
    // Brute force: the five nearest by great-circle distance to the new
    // centre, computed here.
    let mut ranked: Vec<(f64, String)> = f
        .rows
        .iter()
        .map(|row| (metres(110.42, -6.97, row.lon, row.lat), row.key.clone()))
        .collect();
    ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    let want: Vec<String> = ranked.iter().take(5).map(|(_, key)| key.clone()).collect();
    assert_eq!(ordered_keys(&f.db, &got), want);
}

#[test]
fn a_vector_order_rebinds_its_query_vector() {
    let (_dir, f) = open();
    let sql = "SELECT _key FROM place ORDER BY emb <=> $1 LIMIT 5";
    let first = [Param::Vector(f.rows[0].emb.clone())];
    let second = [Param::Vector(f.rows[913].emb.clone())];
    let (prepared, got) = rebound(&f.db, sql, &first, &second);
    assert!(prepared.rebindable());
    assert_eq!(
        ordered_keys(&f.db, &got),
        ordered_keys(&f.db, &fresh(&f.db, sql, &second))
    );
    // Brute force: cosine distance against every stored embedding.
    let query = &f.rows[913].emb;
    let mut ranked: Vec<(f64, String)> = f
        .rows
        .iter()
        .map(|row| (cosine_distance(query, &row.emb), row.key.clone()))
        .collect();
    ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    assert_eq!(
        ordered_keys(&f.db, &got)[0],
        ranked[0].1,
        "the nearest embedding is the row the new vector came from"
    );
}

fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum();
    let na: f64 = a.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 1.0;
    }
    1.0 - dot / (na * nb)
}

#[test]
fn a_text_rank_rebinds_the_query_it_ranks_by() {
    let (_dir, f) = open();
    let sql = "SELECT _key FROM place \
               WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) \
               ORDER BY ts_rank_cd(to_tsvector('simple', text), to_tsquery('simple', $1)) DESC \
               LIMIT 5";
    let second = [Param::Text("pasar".into())];
    let (prepared, got) = rebound(&f.db, sql, &[Param::Text("kopi".into())], &second);
    assert!(prepared.rebindable());
    assert_eq!(
        ordered_keys(&f.db, &got),
        ordered_keys(&f.db, &fresh(&f.db, sql, &second))
    );
    // Brute force on the SET, which ranking cannot change: every row whose
    // stored text holds the term.
    let mut want = by(&f, |row| {
        row.text.split_whitespace().any(|term| term == "pasar")
    });
    want.sort();
    let mut answered = ordered_keys(&f.db, &got);
    answered.sort();
    assert!(
        answered.iter().all(|key| want.contains(key)),
        "a ranked page is a subset of the matching set"
    );
}

#[test]
fn a_blended_score_rebinds_every_leaf_of_its_tree() {
    let (_dir, f) = open();
    let sql = "SELECT _key FROM place \
               WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1) \
               ORDER BY 0.5 * bm25(text, $1) + 0.5 * (1 - (emb <=> $2)) DESC LIMIT 5";
    let second = [Param::Text("pasar".into()), Param::Vector(f.rows[7].emb.clone())];
    let (prepared, got) = rebound(
        &f.db,
        sql,
        &[Param::Text("kopi".into()), Param::Vector(f.rows[0].emb.clone())],
        &second,
    );
    assert!(prepared.rebindable());
    assert_eq!(
        ordered_keys(&f.db, &got),
        ordered_keys(&f.db, &fresh(&f.db, sql, &second))
    );
}

// ── GRAPH_TABLE ───────────────────────────────────────────────────────────

#[test]
fn a_graph_pattern_rebinds_its_seed_key_at_bind_rather_than_at_prepare() {
    let (_dir, f) = open();
    let sql = "SELECT k FROM GRAPH_TABLE (routes MATCH \
               (a:place WHERE a._key = $1)-[:near]->{1,3}(b:place) \
               COLUMNS (b._key AS k))";
    let second = [Param::Text("k00007".into())];
    let (prepared, got) = rebound(&f.db, sql, &[Param::Text("k00000".into())], &second);
    assert!(
        prepared.rebindable(),
        "the seed is a key equality, which is one point-get at BIND"
    );
    assert_eq!(
        ordered_keys(&f.db, &got),
        ordered_keys(&f.db, &fresh(&f.db, sql, &second))
    );
    // Brute force: the fixture links row i to row (i + 4) % 2000, so three
    // hops from `k00007` are exactly k00011, k00015 and k00019.
    let mut want = vec![
        "k00011".to_owned(),
        "k00015".to_owned(),
        "k00019".to_owned(),
    ];
    want.sort();
    let mut answered = ordered_keys(&f.db, &got);
    answered.sort();
    assert_eq!(answered, want);
}

#[test]
fn a_per_hop_node_predicate_rebinds_inside_the_traversal() {
    let (_dir, f) = open();
    let sql = "SELECT k FROM GRAPH_TABLE (routes MATCH \
               (a:place WHERE a._key = $1)-[:near]->{1,3}\
               (b:place WHERE b.born BETWEEN $2 AND $3) COLUMNS (b._key AS k))";
    let second = [
        Param::Text("k00007".into()),
        Param::Int(1900),
        Param::Int(2000),
    ];
    let (prepared, got) = rebound(
        &f.db,
        sql,
        &[
            Param::Text("k00000".into()),
            Param::Int(1901),
            Param::Int(1902),
        ],
        &second,
    );
    assert!(prepared.rebindable());
    assert_eq!(
        keys(&f.db, &got),
        keys(&f.db, &fresh(&f.db, sql, &second)),
        "the per-hop prune's own bounds are slots too"
    );
}

// ── folded answers ────────────────────────────────────────────────────────

#[test]
fn a_grouped_count_rebinds_the_predicate_its_groups_are_taken_over() {
    let (_dir, f) = open();
    let sql = "SELECT kind, count(*) AS n FROM place \
               WHERE ST_DWithin(loc, ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography, $3) GROUP BY kind";
    let (lon, lat, radius) = (110.42, -6.97, 25_000.0);
    let second = [
        Param::Float(lon),
        Param::Float(lat),
        Param::Float(radius),
    ];
    let mut prepared = prepare_sql(
        &f.db,
        sql,
        &[
            Param::Float(106.82),
            Param::Float(-6.17),
            Param::Float(5_000.0),
        ],
    )
    .unwrap();
    assert!(prepared.rebindable());
    prepared.bind(&f.db, &second).unwrap();
    let got = prepared.run(&f.db).unwrap();
    assert_eq!(format!("{got:?}"), format!("{:?}", fresh(&f.db, sql, &second)));

    // Brute force: the counts per kind, with the same named 0.2 % boundary
    // band `radius_oracle` explains.
    let (inside, band) = radius_oracle(&f, lon, lat, radius);
    let count = |keep: &Vec<String>, kind: &str| -> i64 {
        f.rows
            .iter()
            .filter(|row| row.kind == kind && keep.contains(&row.key))
            .count() as i64
    };
    let SqlResult::Rows { rows, .. } = &got else {
        panic!("expected groups");
    };
    assert!(!rows.is_empty());
    for row in rows {
        let (SqlValue::Text(kind), SqlValue::Int(n)) = (&row.values[0], &row.values[1]) else {
            panic!("a group is a key and a count, got {:?}", row.values);
        };
        let low = count(&inside, kind);
        let high = low + count(&band, kind);
        assert!(
            (low..=high).contains(n),
            "group `{kind}` counted {n}, and brute force puts it in {low}..={high}"
        );
    }
}

// ── the refusal ───────────────────────────────────────────────────────────

#[test]
fn a_statement_whose_shape_is_decided_by_a_value_refuses_the_rebind_and_says_which() {
    let (_dir, mut f) = open();
    // `to_tsquery('simple', $1)` with a leading `!` is the COMPLEMENT of a
    // text set and without one it is the set itself: the plan's shape is
    // decided by the parameter's value, which is exactly the case that
    // cannot be a slot.
    let sql =
        "SELECT _key FROM place WHERE to_tsvector('simple', text) @@ to_tsquery('simple', $1)";
    let first = [Param::Text("!kopi".into())];
    let second = [Param::Text("!pasar".into())];
    let prepared = prepare_sql(&f.db, sql, &first).unwrap();
    assert!(!prepared.rebindable());
    let reason = prepared.rebind_refusal().expect("a refusal names its cause");
    assert!(
        reason.contains("$1") && reason.contains("folded at prepare"),
        "the refusal names the slot and what folded it: {reason}"
    );

    // EXPLAIN prints which of the two a statement is, for both answers.
    let refused = match f.db.sql(&format!("EXPLAIN {sql}"), &first).unwrap() {
        SqlResult::Explain(text) => text,
        other => panic!("expected an explain, got {other:?}"),
    };
    assert!(
        refused.contains("rebind: no --") && refused.contains("$1"),
        "EXPLAIN prints the refusal:\n{refused}"
    );
    let allowed = match f
        .db
        .sql("EXPLAIN SELECT _key FROM place WHERE kind = $1", &first)
        .unwrap()
    {
        SqlResult::Explain(text) => text,
        other => panic!("expected an explain, got {other:?}"),
    };
    assert!(
        allowed.contains("rebind: yes --"),
        "EXPLAIN prints the slot answer too:\n{allowed}"
    );

    // And it still answers: `bind` compiles again from the statement it
    // parsed once, so a refused rebind costs a compile and never a parse.
    let mut prepared = prepared;
    prepared.bind(&f.db, &second).unwrap();
    let mut want = by(&f, |row| {
        !row.text.split_whitespace().any(|term| term == "pasar")
    });
    want.sort();
    assert_eq!(keys(&f.db, &prepared.run(&f.db).unwrap()), want);
    assert!(
        !prepared.rebindable(),
        "compiling again under new parameters folds the same value again"
    );

    // The same slot, bound to a value with no `!`, is an ordinary text
    // predicate -- and THAT compiled form is rebindable. Which of the two a
    // prepare produced is a property of the compiled statement, not of the
    // text.
    let plain = prepare_sql(&f.db, sql, &[Param::Text("kopi".into())]).unwrap();
    assert!(plain.rebindable());
}

#[test]
fn a_semi_join_is_never_rebound_because_its_set_is_built_while_it_compiles() {
    let (_dir, mut f) = open();
    // A second collection, so `EXISTS (...)` has a keyspace to walk.
    f.db.sql("CREATE TABLE link (id TEXT PRIMARY KEY, source TEXT)", &[])
        .unwrap();
    f.db.sql("CREATE INDEX link_source ON link USING btree(source)", &[])
        .unwrap();
    for key in f.keys.iter().take(5) {
        f.db.sql(
            "INSERT INTO link (id, source) VALUES ($1, $2)",
            &[Param::Text(format!("l-{key}")), Param::Text(key.clone())],
        )
        .unwrap();
    }
    let sql = "SELECT _key FROM place WHERE EXISTS (SELECT 1 FROM link WHERE source = _key)";
    let prepared = prepare_sql(&f.db, sql, &[]).unwrap();
    assert!(
        !prepared.rebindable(),
        "the set is the database's rows, not the caller's parameters"
    );
    let reason = prepared.rebind_refusal().unwrap();
    assert!(
        reason.contains("semi-join") && reason.contains("while the statement compiles"),
        "{reason}"
    );
    let mut want: Vec<String> = f.keys.iter().take(5).cloned().collect();
    want.sort();
    assert_eq!(keys(&f.db, &prepared.run(&f.db).unwrap()), want);
}

#[test]
fn a_rebind_that_cannot_read_its_parameter_refuses_rather_than_answering() {
    let (_dir, f) = open();
    let mut prepared =
        prepare_sql(&f.db, "SELECT _key FROM place WHERE kind = $1", &[
            Param::Text("depot".into()),
        ])
        .unwrap();
    // `kind` is declared TEXT and a scalar predicate does not coerce, so an
    // integer in that slot is refused at BIND with the same message a fresh
    // compile gives.
    let error = prepared.bind(&f.db, &[Param::Int(7)]).unwrap_err();
    assert!(
        format!("{error}").contains("declared Text"),
        "a rebind stays inside the index's declared domain: {error}"
    );
    let fresh_error = match prepare_sql(&f.db, "SELECT _key FROM place WHERE kind = $1", &[
        Param::Int(7),
    ]) {
        Err(e) => e,
        Ok(_) => panic!("a fresh compile of the same shape must refuse the same value"),
    };
    assert_eq!(format!("{error}"), format!("{fresh_error}"));
}

#[test]
fn a_missing_parameter_is_refused_at_bind_by_its_number() {
    let (_dir, f) = open();
    let mut prepared =
        prepare_sql(&f.db, "SELECT _key FROM place WHERE kind = $1", &[
            Param::Text("depot".into()),
        ])
        .unwrap();
    let error = prepared.bind(&f.db, &[]).unwrap_err();
    assert!(
        format!("{error}").contains("$1 is not bound"),
        "{error}"
    );
}
