//! Path and element functions, list literals and horizontal aggregation in
//! a GQL body (M4-D of `docs/lang/GQL_PROFILE_DESIGN.md` §4.4, §4.5, §6.3;
//! owner answers Q2, Q9, Q16), driven through `prepare_sql`.
//!
//! What is at risk, and the test that pins it:
//!
//! * "Horizontal vs vertical aggregation": `SUM(e.w)` in a `LET` folds the
//!   edges of ONE path, and `SUM` in the `RETURN` folds the path ROWS, so a
//!   path sum differs from a sum over paths; a group variable outside its
//!   quantifier never reads as its last edge (`horizontal_*`);
//! * brief §6's horizontal example -- a strongest chain as `EXP(SUM(LN(w)))`
//!   per path, then `MAX` per destination -- on a neutral graph
//!   (`the_strongest_chain_*`);
//! * empty and null folds: a zero-length path, and edges whose property is
//!   null or missing (`a_fold_over_*`);
//! * horizontal context rules: `LET`, `FILTER` and the `MATCH`'s `WHERE`
//!   only, never a `RETURN` aggregate (`horizontal_context_*`);
//! * the path and element functions, their compile-time types and their
//!   argument kinds (`path_functions_*`, `element_functions_*`);
//! * list literals, typed at compile time, a mixed-kind list refused by name
//!   (`list_literals_*`).
//!
//! Workload names are invented: collections `site_a`, `site_b`, edge types
//! `r` and `c`, keys `n0`.., `t0`.., `c0`, `c1`, `m0`, `m1`.

use sekejap_core::collections::{Database, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, prepare_sql, Param, SqlError, SqlResult, SqlValue};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// site_a (w: BIGINT, declared), edge type r (bag {"w": ..}):
///   chain    n0 -1-> n1 -2-> n2 -4-> n3        direct  n0 -10-> n3
///   nulls    n3 -null-> n4 -(no w)-> n5
///   cycle    c0 -1-> c1 -1-> c0
///   props    m0 {w: 1, extra: null}   m1 {}   (no edges)
/// site_b, edge type c (bag {"w": confidence}):
///   t0 -0.5-> t1 -0.5-> t2      t0 -0.2-> t2
///   t1 -0.9-> t3                t2 -1.0-> t3      t3 -0.5-> t0
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let site_a = db
        .create_collection(
            "site_a",
            vec![("w".to_owned(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    let site_b = db
        .create_collection(
            "site_b",
            vec![("w".to_owned(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    let mut a = std::collections::HashMap::new();
    for (key, w) in [
        ("n0", 0),
        ("n1", 1),
        ("n2", 2),
        ("n3", 3),
        ("n4", 4),
        ("n5", 5),
        ("c0", 0),
        ("c1", 1),
    ] {
        a.insert(key, db.put(site_a, key, &json!({"w": w})).unwrap());
    }
    db.put(site_a, "m0", &json!({"w": 1, "extra": null}))
        .unwrap();
    db.put(site_a, "m1", &json!({})).unwrap();
    let mut b = std::collections::HashMap::new();
    for key in ["t0", "t1", "t2", "t3"] {
        b.insert(key, db.put(site_b, key, &json!({"w": 0})).unwrap());
    }
    db.enable_graph().unwrap();
    let r = db.create_edge_type("r").unwrap();
    let c = db.create_edge_type("c").unwrap();
    for (from, to, bag) in [
        ("n0", "n1", json!({"w": 1})),
        ("n1", "n2", json!({"w": 2})),
        ("n2", "n3", json!({"w": 4})),
        ("n0", "n3", json!({"w": 10})),
        ("n3", "n4", json!({"w": null})),
        ("n4", "n5", json!({})),
        ("c0", "c1", json!({"w": 1})),
        ("c1", "c0", json!({"w": 1})),
    ] {
        db.create_edge(GraphContextId::BASE, a[from], r, a[to], &bag)
            .unwrap();
    }
    for (from, to, w) in [
        ("t0", "t1", 0.5),
        ("t1", "t2", 0.5),
        ("t0", "t2", 0.2),
        ("t1", "t3", 0.9),
        ("t2", "t3", 1.0),
        ("t3", "t0", 0.5),
    ] {
        db.create_edge(GraphContextId::BASE, b[from], c, b[to], &json!({"w": w}))
            .unwrap();
    }
    db.commit().unwrap();
    db
}

#[derive(Debug)]
struct Answer {
    types: Vec<&'static str>,
    rows: Vec<Vec<SqlValue>>,
}

fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (base {body})")
}

fn run_with(db: &Database, body: &str, params: &[Param]) -> Result<Answer, SqlError> {
    let text = statement(body);
    let prepared = prepare_sql(db, &text, params)?;
    let types = (0..prepared.columns().len())
        .map(|at| prepared.column_type(at).expect("a GQL column is typed"))
        .collect();
    match prepared.run(db)? {
        SqlResult::Rows { rows, .. } => Ok(Answer {
            types,
            rows: rows.into_iter().map(|row| row.values).collect(),
        }),
        other => panic!("`{text}` answered {other:?}"),
    }
}

fn run(db: &Database, body: &str) -> Answer {
    run_with(db, body, &[]).unwrap_or_else(|error| panic!("`{body}` failed: {error}"))
}

fn error(db: &Database, body: &str) -> String {
    match run_with(db, body, &[]) {
        Ok(answer) => panic!("`{body}` answered {:?} instead of failing", answer.rows),
        Err(error) => error.to_string(),
    }
}

/// A cell as text: a list as its JSON array, a float with two decimals.
fn cell(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "NULL".to_owned(),
        SqlValue::Bool(b) => b.to_string(),
        SqlValue::Int(i) => i.to_string(),
        SqlValue::Float(f) => format!("{f:.2}"),
        SqlValue::Text(t) => t.clone(),
        SqlValue::Json(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

/// The answer as sorted `a|b|c` rows: a pattern answer is a bag.
fn bag(answer: &Answer) -> Vec<String> {
    let mut rows: Vec<String> = answer
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect::<Vec<_>>().join("|"))
        .collect();
    rows.sort();
    rows
}

/// Every path from n0 to n3 over `r`: the direct edge and the chain.
const TO_N3: &str = "MATCH p = (a:site_a WHERE a._key = 'n0')-[e:r]->{1,3}(b WHERE b._key = 'n3')";

// ── horizontal versus vertical ─────────────────────────────────────────────

#[test]
fn horizontal_a_path_sum_differs_from_a_sum_over_paths() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Per path: the direct edge sums to 10, the chain to 1 + 2 + 4 = 7.
    let per_path = run(
        &db,
        &format!("{TO_N3} LET total = SUM(e.w) RETURN PATH_LENGTH(p) AS hops, total"),
    );
    assert_eq!(bag(&per_path), ["1|10", "3|7"]);
    // An edge's bag is undeclared (Q8), so its SUM is declared DOUBLE
    // PRECISION; integers still sum exactly, as the vertical SUM does.
    assert_eq!(per_path.types, ["BIGINT", "DOUBLE PRECISION"]);
    // Over paths: the RETURN folds the two ROWS.
    let over_paths = run(
        &db,
        &format!("{TO_N3} LET total = SUM(e.w) RETURN SUM(total) AS all_paths, COUNT(*) AS paths, MAX(total) AS best"),
    );
    assert_eq!(bag(&over_paths), ["17|2|10"]);
    // Every horizontal aggregate, over the chain 1, 2, 4.
    let chain = run(
        &db,
        &format!(
            "{TO_N3} FILTER PATH_LENGTH(p) = 3 \
             LET n = COUNT(e), nw = COUNT(e.w), lo = MIN(e.w), hi = MAX(e.w), mean = AVG(e.w), ws = ARRAY_AGG(e.w) \
             RETURN n, nw, lo, hi, mean, ws"
        ),
    );
    assert_eq!(bag(&chain), ["3|3|1|4|2.33|[1,2,4]"]);
}

#[test]
fn horizontal_a_group_variable_is_never_its_last_edge() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // The chain's last edge has w = 4; nothing reads it as the value of `e`.
    for body in [
        format!("{TO_N3} LET w = e.w RETURN w"),
        format!("{TO_N3} RETURN e.w AS w"),
        format!("{TO_N3} FILTER e.w = 4 RETURN PATH_LENGTH(p) AS hops"),
        format!("{TO_N3} WHERE e.w = 4 RETURN PATH_LENGTH(p) AS hops"),
    ] {
        let message = error(&db, &body);
        assert!(
            message.contains("group variable") && message.contains("horizontal aggregate"),
            "`{body}`: {message}"
        );
    }
    // A RETURN aggregate is vertical: over a group variable it points to a LET.
    let message = error(&db, &format!("{TO_N3} RETURN SUM(e.w) AS total"));
    assert!(message.contains("LET"), "{message}");
}

#[test]
fn horizontal_aggregates_filter_in_the_match_where_and_in_filter() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        &format!("{TO_N3} WHERE SUM(e.w) < 9 RETURN PATH_LENGTH(p) AS hops"),
    );
    assert_eq!(bag(&answer), ["3"]);
    let answer = run(
        &db,
        &format!("{TO_N3} FILTER MAX(e.w) >= 10 RETURN PATH_LENGTH(p) AS hops"),
    );
    assert_eq!(bag(&answer), ["1"]);
    // Over the nodes of the path, through NODES. The path's intermediate
    // nodes may be in any collection, so `ns.w` is undecided at compile
    // (Q8) and its SUM declared DOUBLE PRECISION.
    let answer = run(
        &db,
        &format!(
            "{TO_N3} LET ns = NODES(p) LET s = SUM(ns.w), keys = ARRAY_AGG(ns._key) RETURN s, keys"
        ),
    );
    assert_eq!(
        bag(&answer),
        ["3|[\"n0\",\"n3\"]", "6|[\"n0\",\"n1\",\"n2\",\"n3\"]"]
    );
    assert_eq!(answer.types, ["DOUBLE PRECISION", "TEXT[]"]);
}

#[test]
fn horizontal_count_distinct_counts_identities_along_a_walk() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // c0 -> c1 -> c0 -> c1: three edge crossings, two distinct edges.
    let answer = run(
        &db,
        "MATCH (a:site_a WHERE a._key = 'c0')-[e:r]->{3}(b) \
         LET n = COUNT(e), d = COUNT(DISTINCT e) RETURN b._key AS k, n, d",
    );
    assert_eq!(bag(&answer), ["c1|3|2"]);
}

#[test]
fn horizontal_aggregates_fold_scalar_lists_too() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "LET xs = [1, 2, 3] LET s = SUM(xs), twice = SUM(xs * 2), n = COUNT(xs) RETURN s, twice, n",
    );
    assert_eq!(bag(&answer), ["6|12|3"]);
    assert_eq!(answer.types, ["BIGINT", "BIGINT", "BIGINT"]);
    // A list an earlier stage aggregated, folded per row in the next.
    let answer = run(
        &db,
        "MATCH (a:site_a WHERE a._key = 'n0')-[:r]->(b) RETURN ARRAY_AGG(b.w) AS ws \
         NEXT LET s = SUM(ws), n = ARRAY_LENGTH(ws) RETURN s, n",
    );
    assert_eq!(bag(&answer), ["4|2"]);
}

#[test]
fn horizontal_context_is_let_filter_and_the_match_where_only() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (body, says) in [
        // An aggregate over no list variable is vertical, and misplaced.
        ("MATCH (a:site_a) LET s = SUM(a.w) RETURN s".to_owned(), "list"),
        // Nested.
        (format!("{TO_N3} LET s = SUM(COUNT(e)) RETURN s"), "inside another aggregate"),
        // A vertical aggregate never folds horizontally, nor holds a fold.
        (format!("{TO_N3} RETURN MAX(SUM(e.w)) AS m"), "LET"),
        // Two lists in one argument.
        (format!("{TO_N3} LET ns = NODES(p) LET s = SUM(ns.w + e.w) RETURN s"), "one list"),
        // Not in an inline predicate: during the search it would see one edge.
        (
            "MATCH (a:site_a WHERE a._key = 'n0')-[e:r]->{1,3}(b WHERE SUM(e.w) > 3) RETURN b._key AS k".to_owned(),
            "LET",
        ),
        // Not a FOR list.
        (format!("{TO_N3} FOR x IN ARRAY_AGG(e.w) RETURN x"), "LET"),
    ] {
        let message = error(&db, &body);
        assert!(message.contains(says), "`{body}`: {message}");
    }
}

// ── brief §6's horizontal example ──────────────────────────────────────────

#[test]
fn the_strongest_chain_is_a_horizontal_product_then_a_vertical_max() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let text = statement(
        "MATCH p = ACYCLIC (start:site_b WHERE start._key = $1) \
           -[e:c WHERE e.w > 0 AND e.w <= 1]->{1,8}(destination:site_b) \
         LET chain_weight = EXP(SUM(LN(e.w))) \
         RETURN destination._key AS destination_key, MAX(chain_weight) AS best_chain_weight, COUNT(*) AS paths \
         GROUP BY destination._key",
    );
    let prepared = prepare_sql(&db, &text, &[Param::Text("t0".into())]).unwrap();
    assert_eq!(prepared.column_type(1), Some("DOUBLE PRECISION"));
    let SqlResult::Rows { rows, .. } = prepared.run(&db).unwrap() else {
        panic!("not rows")
    };
    let mut got: Vec<(String, f64, i64)> = rows
        .iter()
        .map(|row| match &row.values[..] {
            [SqlValue::Text(k), SqlValue::Float(w), SqlValue::Int(n)] => (k.clone(), *w, *n),
            other => panic!("{other:?}"),
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    // t1: 0.5. t2: 0.5 * 0.5 beats the direct 0.2. t3: 0.5 * 0.9 beats
    // 0.25 * 1.0 and 0.2 * 1.0. ACYCLIC never returns to t0.
    let want = [("t1", 0.5, 1), ("t2", 0.25, 2), ("t3", 0.45, 3)];
    assert_eq!(got.len(), want.len(), "{got:?}");
    for ((key, weight, paths), (k, w, n)) in got.iter().zip(want) {
        assert_eq!((key.as_str(), *paths), (k, n));
        assert!((weight - w).abs() < 1e-9, "{key}: {weight} != {w}");
    }
}

#[test]
fn selected_paths_fold_their_witness_brief_8_1_and_8_2() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let params = [Param::Text("n0".into()), Param::Text("n3".into())];
    // Fewest hops: the direct edge.
    let answer = run_with(
        &db,
        "MATCH p = ANY SHORTEST (source:site_a WHERE source._key = $1) \
           -[e:r]->{0,32}(target:site_a WHERE target._key = $2) \
         LET ns = NODES(p) \
         LET node_keys = ARRAY_AGG(ns._key) \
         RETURN source._key AS source_key, target._key AS target_key, PATH_LENGTH(p) AS hops, node_keys",
        &params,
    )
    .unwrap();
    assert_eq!(bag(&answer), ["n0|n3|1|[\"n0\",\"n3\"]"]);
    assert_eq!(answer.types, ["TEXT", "TEXT", "BIGINT", "TEXT[]"]);
    // Lowest total cost, t0 to t3: t0 -0.2-> t2 -1.0-> t3 (1.2) against
    // t0 -0.5-> t1 -0.9-> t3 (1.4) and the three-hop 2.0.
    let answer = run_with(
        &db,
        "MATCH p = ANY CHEAPEST (source:site_b WHERE source._key = $1) \
           -[e:c COST e.w]->{0,32}(target:site_b WHERE target._key = $2) \
         LET total_cost = COALESCE(SUM(e.w), 0.0) \
         RETURN PATH_LENGTH(p) AS hops, total_cost",
        &[Param::Text("t0".into()), Param::Text("t3".into())],
    )
    .unwrap();
    assert_eq!(bag(&answer), ["2|1.20"]);
    assert_eq!(answer.types, ["BIGINT", "DOUBLE PRECISION"]);
}

// ── empty and null folds ───────────────────────────────────────────────────

#[test]
fn a_fold_over_a_zero_length_path_is_count_zero_null_and_an_empty_list() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "MATCH p = (a:site_a WHERE a._key = 'n0')-[e:r]->{0,1}(b WHERE b._key = 'n0') \
         LET n = COUNT(e), s = SUM(e.w), total = COALESCE(SUM(e.w), 0.0), ws = ARRAY_AGG(e.w) \
         RETURN PATH_LENGTH(p) AS hops, n, s, total, ws, ARRAY_LENGTH(ws) AS len, ARRAY_LENGTH(NODES(p)) AS nodes",
    );
    assert_eq!(bag(&answer), ["0|0|NULL|0.00|[]|0|1"]);
    assert_eq!(
        answer.types,
        [
            "BIGINT",
            "BIGINT",
            "DOUBLE PRECISION",
            "DOUBLE PRECISION",
            "TEXT[]",
            "BIGINT",
            "BIGINT"
        ]
    );
}

#[test]
fn a_fold_over_null_and_missing_properties_skips_them_as_sql_does() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // n3 -> n4 holds w = null; n4 -> n5 holds no w.
    let answer = run(
        &db,
        "MATCH (a:site_a WHERE a._key = 'n3')-[e:r]->{2}(b) \
         LET n = COUNT(e), nw = COUNT(e.w), s = SUM(e.w), lo = MIN(e.w), ws = ARRAY_AGG(e.w) \
         RETURN b._key AS k, n, nw, s, lo, ws",
    );
    assert_eq!(bag(&answer), ["n5|2|0|NULL|NULL|[null,null]"]);
}

// ── path functions ─────────────────────────────────────────────────────────

#[test]
fn path_functions_read_the_path_the_search_built() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        &format!(
            "{TO_N3} LET ns = NODES(p), es = EDGES(p) \
             LET keys = ARRAY_AGG(ns._key), ws = ARRAY_AGG(es.w) \
             RETURN PATH_LENGTH(p) AS hops, PATH_FIRST(p) = a AS first_is_a, PATH_LAST(p) = b AS last_is_b, \
                    ELEMENT_ID(PATH_FIRST(p)) = ELEMENT_ID(a) AS same_id, keys, ws, \
                    ARRAY_LENGTH(ns) AS node_count, ARRAY_LENGTH(es) AS edge_count, \
                    IS_ACYCLIC(p) AS acyclic, IS_TRAIL(p) AS trail"
        ),
    );
    assert_eq!(
        bag(&answer),
        [
            "1|true|true|true|[\"n0\",\"n3\"]|[10]|2|1|true|true",
            "3|true|true|true|[\"n0\",\"n1\",\"n2\",\"n3\"]|[1,2,4]|4|3|true|true",
        ]
    );
    assert_eq!(
        answer.types,
        [
            "BIGINT", "BOOLEAN", "BOOLEAN", "BOOLEAN", "TEXT[]", "TEXT[]", "BIGINT", "BIGINT",
            "BOOLEAN", "BOOLEAN"
        ]
    );
}

#[test]
fn path_functions_is_acyclic_and_is_trail_inspect_repetition() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // WALK over the two-cycle c0 <-> c1.
    let answer = run(
        &db,
        "MATCH p = (a:site_a WHERE a._key = 'c0')-[:r]->{1,4}(b) \
         RETURN PATH_LENGTH(p) AS hops, IS_ACYCLIC(p) AS acyclic, IS_TRAIL(p) AS trail",
    );
    assert_eq!(
        bag(&answer),
        [
            "1|true|true",
            "2|false|true",
            "3|false|false",
            "4|false|false"
        ]
    );
}

#[test]
fn path_functions_refuse_an_argument_of_the_wrong_kind_at_compile() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for (body, says) in [
        (
            "MATCH (a:site_a) RETURN PATH_LENGTH(a) AS n".to_owned(),
            "PATH_LENGTH",
        ),
        (
            "MATCH (a:site_a) RETURN IS_TRAIL(a.w) AS n".to_owned(),
            "IS_TRAIL",
        ),
        (format!("{TO_N3} RETURN ELEMENT_ID(p) AS n"), "ELEMENT_ID"),
        (
            "MATCH (a:site_a) RETURN SOURCE_NODE_ID(a) AS n".to_owned(),
            "SOURCE_NODE_ID",
        ),
        (
            "MATCH (a:site_a) RETURN ARRAY_LENGTH(a.w) AS n".to_owned(),
            "ARRAY_LENGTH",
        ),
        (
            "MATCH (a:site_a) RETURN LABELS($1) AS n".to_owned(),
            "LABELS",
        ),
        // A list of nodes is not an output column.
        (format!("{TO_N3} RETURN NODES(p) AS ns"), "holds a node"),
        // A property of a list is read only inside a fold.
        (format!("{TO_N3} LET ns = NODES(p) RETURN ns.w AS w"), "ns"),
    ] {
        let message = error(&db, &body);
        assert!(message.contains(says), "`{body}`: {message}");
    }
    // NULL in, NULL out.
    let answer = run(
        &db,
        "RETURN PATH_LENGTH(NULL) AS a, ELEMENT_ID(NULL) AS b, ARRAY_LENGTH(NULL) AS c, LABELS(NULL) AS d",
    );
    assert_eq!(bag(&answer), ["NULL|NULL|NULL|NULL"]);
    assert_eq!(answer.types, ["BIGINT", "TEXT", "BIGINT", "TEXT[]"]);
}

#[test]
fn path_functions_giving_elements_are_not_columns_or_sort_keys() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Refused when the statement is compiled, before any row exists.
    let compile_error = |body: String| match prepare_sql(&db, &statement(&body), &[]) {
        Ok(_) => panic!("`{body}` compiled"),
        Err(error) => error.to_string(),
    };
    let message = compile_error(format!("{TO_N3} RETURN PATH_FIRST(p) AS s"));
    assert!(message.contains("`s` is a node"), "{message}");
    let message = compile_error(format!(
        "{TO_N3} RETURN PATH_LENGTH(p) AS hops ORDER BY PATH_LAST(p)"
    ));
    assert!(message.contains("ELEMENT_ID"), "{message}");
    // Through NEXT a node stays a node, and its property is a column.
    let answer = run(
        &db,
        &format!("{TO_N3} RETURN PATH_LAST(p) AS last NEXT RETURN last._key AS k"),
    );
    assert_eq!(bag(&answer), ["n3", "n3"]);
}

#[test]
fn horizontal_a_return_aggregate_never_sums_a_list() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // SUM over the rows of a list-valued column would be a horizontal
    // fold in disguise: it is refused, pointing to a LET.
    let text = statement(&format!(
        "{TO_N3} LET ws = ARRAY_AGG(e.w) RETURN SUM(ws) AS total"
    ));
    let message = prepare_sql(&db, &text, &[]).err().expect("refused at compile").to_string();
    assert!(message.contains("LET"), "{message}");
}

// ── element functions ──────────────────────────────────────────────────────

#[test]
fn element_functions_source_and_destination_are_the_stored_endpoints() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Crossed backwards: the stored source is y, not x.
    let answer = run(
        &db,
        "MATCH (x:site_a WHERE x._key = 'n1')<-[e:r]-(y) \
         RETURN y._key AS k, SOURCE_NODE_ID(e) = ELEMENT_ID(y) AS src, \
                DESTINATION_NODE_ID(e) = ELEMENT_ID(x) AS dst, SOURCE_NODE_ID(e) AS id",
    );
    assert_eq!(answer.rows.len(), 1);
    assert_eq!(
        bag(&answer)[0].split('|').take(3).collect::<Vec<_>>(),
        ["n0", "true", "true"]
    );
    assert_eq!(answer.types, ["TEXT", "BOOLEAN", "BOOLEAN", "TEXT"]);
}

#[test]
fn element_functions_element_id_is_unique_within_a_query() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "MATCH (a:site_a)-[e:r]->(b) RETURN ELEMENT_ID(a) AS na, ELEMENT_ID(e) AS ne, ELEMENT_ID(b) AS nb",
    );
    let mut nodes = std::collections::HashMap::new();
    let mut edges = std::collections::HashSet::new();
    for row in &answer.rows {
        let [SqlValue::Text(na), SqlValue::Text(ne), SqlValue::Text(nb)] = &row[..] else {
            panic!("{row:?}")
        };
        assert!(edges.insert(ne.clone()), "edge id {ne} twice");
        assert_ne!(na, ne);
        nodes.insert(na.clone(), ());
        nodes.insert(nb.clone(), ());
    }
    assert_eq!(edges.len(), 8);
    // n0 n1 n2 n3 n4 n5 c0 c1.
    assert_eq!(nodes.len(), 8);
    // The same node reached twice has one id.
    let answer = run(
        &db,
        "MATCH (a:site_a WHERE a._key = 'n0')-[:r]->(b), (c:site_a WHERE c._key = 'n2')-[:r]->(d) \
         RETURN ELEMENT_ID(d) = ELEMENT_ID(b) AS same, b._key AS k",
    );
    assert_eq!(bag(&answer), ["false|n1", "true|n3"]);
}

#[test]
fn element_functions_labels_and_property_names_keep_presence() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let names = |key: &str| {
        run_with(
            &db,
            "MATCH (m:site_a WHERE m._key = $1) \
             RETURN m._key AS k, LABELS(m) AS labels, PROPERTY_NAMES(m) AS names, m.extra IS NULL AS extra_null",
            &[Param::Text(key.into())],
        )
        .unwrap()
    };
    // m0 stores `extra` as null: present, though it reads as NULL.
    let answer = names("m0");
    assert_eq!(
        bag(&answer),
        ["m0|[\"site_a\"]|[\"_key\",\"extra\",\"w\"]|true"]
    );
    assert_eq!(answer.types, ["TEXT", "TEXT[]", "TEXT[]", "BOOLEAN"]);
    assert_eq!(bag(&names("m1")), ["m1|[\"site_a\"]|[\"_key\"]|true"]);
    let answer = run(
        &db,
        "MATCH (a:site_a WHERE a._key = 'n3')-[e:r]->{2}(b) \
         LET names = ARRAY_AGG(PROPERTY_NAMES(e)), labels = ARRAY_AGG(LABELS(e)) \
         FOR x IN names RETURN ARRAY_LENGTH(x) AS n",
    );
    // n3 -> n4 stores `w: null` (one name), n4 -> n5 stores nothing.
    assert_eq!(bag(&answer), ["0", "1"]);
}

// ── list literals ──────────────────────────────────────────────────────────

#[test]
fn list_literals_are_typed_at_compile_time() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "RETURN [1, 2, 3] AS ints, [1, 2.5] AS floats, ['x', NULL] AS texts, [TRUE] AS bools, [] AS empty, \
                ARRAY_LENGTH([1, NULL, 3]) AS len",
    );
    assert_eq!(bag(&answer), ["[1,2,3]|[1.0,2.5]|[\"x\",null]|[true]|[]|3"]);
    assert_eq!(
        answer.types,
        [
            "BIGINT[]",
            "DOUBLE PRECISION[]",
            "TEXT[]",
            "BOOLEAN[]",
            "TEXT[]",
            "BIGINT"
        ]
    );
    let answer = run(&db, "FOR x IN [3, 1, 2] RETURN x ORDER BY x");
    assert_eq!(
        answer.rows,
        [[SqlValue::Int(1)], [SqlValue::Int(2)], [SqlValue::Int(3)]]
    );
    assert_eq!(answer.types, ["BIGINT"]);
    // Elements, as a value: unnested, each is the node again.
    let answer = run(
        &db,
        "MATCH (a:site_a WHERE a._key = 'n0')-[:r]->(b:site_a) FOR x IN [a, b] RETURN x._key AS k",
    );
    assert_eq!(bag(&answer), ["n0", "n0", "n1", "n3"]);
    // A property typed at compile: a declared BIGINT and a float promote.
    let answer = run(
        &db,
        "MATCH (a:site_a WHERE a._key = 'n2') RETURN [a.w, 0.5] AS xs, [a._key] AS ks",
    );
    assert_eq!(bag(&answer), ["[2.0,0.5]|[\"n2\"]"]);
    assert_eq!(answer.types, ["DOUBLE PRECISION[]", "TEXT[]"]);
}

#[test]
fn list_literals_refuse_mixed_kinds_by_name() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        "RETURN [1, 'x'] AS xs",
        "MATCH (a:site_a WHERE a._key = 'n0') RETURN [a._key, a.w] AS xs",
        "MATCH (a:site_a WHERE a._key = 'n0')-[e:r]->(b) LET xs = [a, e] RETURN b._key AS k",
    ] {
        let message = error(&db, body);
        assert!(message.contains("list literal"), "`{body}`: {message}");
    }
    // A list of lists is not a final column (a PostgreSQL array is
    // rectangular).
    let message = error(&db, "RETURN [[1], [2]] AS xs");
    assert!(message.contains("list of lists"), "{message}");
    // A parameter takes the list's kind, checked when the list is built.
    let answer = run_with(&db, "RETURN [$1, 2] AS xs", &[Param::Int(1)]).unwrap();
    assert_eq!(bag(&answer), ["[1,2]"]);
    let message = run_with(&db, "RETURN [$1, 2] AS xs", &[Param::Text("x".into())])
        .unwrap_err()
        .to_string();
    assert!(message.contains("list"), "{message}");
}

#[test]
fn explain_prints_folds_and_functions() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let text = statement(&format!(
        "{TO_N3} LET total = SUM(e.w), ids = [ELEMENT_ID(a), ELEMENT_ID(b)] RETURN total, PATH_LENGTH(p) AS hops"
    ));
    let plan = explain_sql(&db, &text, &[]).unwrap();
    assert!(plan.contains("SUM(e.w)"), "{plan}");
    assert!(plan.contains("[ELEMENT_ID(a), ELEMENT_ID(b)]"), "{plan}");
    assert!(plan.contains("PATH_LENGTH(p)"), "{plan}");
}
