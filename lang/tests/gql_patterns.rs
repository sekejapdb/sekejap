//! GQL patterns through the SQL entry points (`prepare_sql`,
//! `PreparedSql::run`, `Database::sql`): the binder, the planner and the
//! engine's cursor behind `Plan::Gql` (M2-C and M2-D of
//! `docs/lang/GQL_PROFILE_DESIGN.md`). The work an answer charged is read
//! from its `EXPLAIN`, which runs the statement and prints the counters.
//!
//! What is at risk, and the test that pins it:
//!
//! * the opening example: a key seed, an incoming then an outgoing hop, and
//!   properties of TWO variables returned per row (`the_opening_example_*`);
//! * multi-hop chains, and a seed in the MIDDLE of a chain, which walks the
//!   left part against its written direction (`a_three_hop_chain_*`,
//!   `a_seed_in_the_middle_*`);
//! * comma patterns: a shared variable re-seeds as the bound node, and a
//!   second pattern whose far end is already bound is an `ExpandInto`
//!   (`comma_patterns_*`); no shared variable is a cross product;
//! * label alternation on nodes and on edges (`label_alternation_*`);
//! * a repeated variable is ONE element, for nodes and for edges
//!   (`a_repeated_*`);
//! * a key that no row holds, and a `NULL` key, give zero rows, not an
//!   error (`a_missing_seed_*`);
//! * two collections holding the same external key are two nodes
//!   (`two_collections_sharing_a_key_*`);
//! * an inline element predicate prunes PER HOP and a pattern `WHERE` runs
//!   after the pattern: the same answer, measurably different work
//!   (`inline_and_pattern_where_*`);
//! * three-valued logic: a comparison with a missing property is unknown,
//!   and `NOT unknown` is unknown (`three_valued_logic_*`);
//! * the seed choice: a scalar index answers a label-scan predicate, and a
//!   predicate no index answers is refused naming the index (design Q5)
//!   (`an_indexed_*`, `an_unindexed_*`);
//! * the graph argument names a context; `base` is the base graph (Q15);
//! * scope errors name the variable (`scope_errors_*`);
//! * the SQL surface of a GQL plan (M2-D): rows carry no owner, pages equal
//!   the one-shot answer, a bind swaps the parameters and compiles nothing,
//!   and columns and parameters are typed from the binding schema
//!   (`rows_carry_*`, `a_bind_*`, `columns_are_typed_*`,
//!   `parameters_are_typed_*`). The outer SELECT over the relation (M3-D)
//!   is `gql_prepared.rs`.
//!
//! Workload names are invented: bands `b1` `b2`, people `p1`-`p3`, songs
//! `s1`-`s4`, a venue `b1` that shares a band's key.

use sekejap_core::collections::{Database, EntityId, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{
    explain_sql, prepare_sql, Param, SqlDatabase, SqlError, SqlResult, SqlRow, SqlValue,
};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// band   b1 "band one", b2 "band two"          venue b1 "venue one"
/// person p1 age 41, p2 age 25, p3 (no age)     person.age is indexed
/// song   s1..s4, year 2001..2004               song.year is NOT indexed
///
/// member_of  p1->b1  p2->b1  p3->b2
/// performed  p1->s1 {times 3}  p1->s2 {times 1}  p2->s1 {times 5}  p3->s3 {}
/// wrote      p1->s1  p3->s3  p2->s4
/// in ctx_a:  performed p2->s2
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let int = |name: &str| (name.to_owned(), Kind::Int);
    let band = db
        .create_collection("band", vec![text("name")], Default::default())
        .unwrap();
    let venue = db
        .create_collection("venue", vec![text("name")], Default::default())
        .unwrap();
    let person = db
        .create_collection("person", vec![text("name"), int("age")], Default::default())
        .unwrap();
    let song = db
        .create_collection("song", vec![text("title"), int("year")], Default::default())
        .unwrap();
    let b1 = db.put(band, "b1", &json!({"name": "band one"})).unwrap();
    let b2 = db.put(band, "b2", &json!({"name": "band two"})).unwrap();
    db.put(venue, "b1", &json!({"name": "venue one"})).unwrap();
    let p1 = db
        .put(person, "p1", &json!({"name": "person one", "age": 41}))
        .unwrap();
    let p2 = db
        .put(person, "p2", &json!({"name": "person two", "age": 25}))
        .unwrap();
    let p3 = db.put(person, "p3", &json!({"name": "person three"})).unwrap();
    let mut s = Vec::new();
    for i in 1..=4 {
        s.push(
            db.put(
                song,
                &format!("s{i}"),
                &json!({"title": format!("song {i}"), "year": 2000 + i}),
            )
            .unwrap(),
        );
    }
    let age = db.create_scalar_index(person, "person_age", "age", false).unwrap();
    while !db.build_index_step(age, 64).unwrap() {
        db.commit().unwrap();
    }
    db.enable_graph().unwrap();
    let member_of = db.create_edge_type("member_of").unwrap();
    let performed = db.create_edge_type("performed").unwrap();
    let wrote = db.create_edge_type("wrote").unwrap();
    let ctx_a = db.create_graph_context("ctx_a").unwrap();
    let base = GraphContextId::BASE;
    let edges: [(GraphContextId, EntityId, _, EntityId, serde_json::Value); 11] = [
        (base, p1, member_of, b1, json!({})),
        (base, p2, member_of, b1, json!({})),
        (base, p3, member_of, b2, json!({})),
        (base, p1, performed, s[0], json!({"times": 3})),
        (base, p1, performed, s[1], json!({"times": 1})),
        (base, p2, performed, s[0], json!({"times": 5})),
        (base, p3, performed, s[2], json!({})),
        (base, p1, wrote, s[0], json!({})),
        (base, p3, wrote, s[2], json!({})),
        (base, p2, wrote, s[3], json!({})),
        (ctx_a, p2, performed, s[1], json!({})),
    ];
    for (context, source, edge_type, destination, bag) in edges {
        db.create_edge(context, source, edge_type, destination, &bag)
            .unwrap();
    }
    db.commit().unwrap();
    db
}

/// One GQL answer through the SQL entry point: the columns, each row's
/// values, and the notices the prepare raised.
#[derive(Debug)]
struct Answer {
    columns: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
    notices: Vec<String>,
}

fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE ({body})")
}

/// Every row of a relation carries no owner (design Q1).
fn values(rows: Vec<SqlRow>) -> Vec<Vec<SqlValue>> {
    rows.into_iter()
        .map(|row| {
            assert_eq!(row.owner(), None, "a GQL row has no single owner: {row:?}");
            row.values
        })
        .collect()
}

fn run_with(db: &Database, body: &str, params: &[Param]) -> Result<Answer, SqlError> {
    let prepared = prepare_sql(db, &statement(body), params)?;
    let SqlResult::Rows { columns, rows } = prepared.run(db)? else {
        panic!("`{body}` did not answer with rows");
    };
    Ok(Answer {
        columns,
        rows: values(rows),
        notices: prepared.notices().to_vec(),
    })
}

fn run(db: &Database, body: &str, params: &[Param]) -> Answer {
    run_with(db, body, params).unwrap_or_else(|error| panic!("`{body}` failed: {error}"))
}

/// One work counter of an execution, read off the statement's `EXPLAIN`,
/// which runs it once.
fn work(db: &Database, body: &str, params: &[Param], name: &str) -> u64 {
    let text = explain_sql(db, &statement(body), params)
        .unwrap_or_else(|error| panic!("EXPLAIN `{body}` failed: {error}"));
    let line = text
        .lines()
        .find(|line| line.starts_with("work: "))
        .unwrap_or_else(|| panic!("no work line in:\n{text}"));
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no `{name}` counter in:\n{text}"))
        .parse()
        .unwrap()
}

/// The answer's rows as text, sorted: a pattern answer is a BAG, so the
/// order is not part of it but the multiplicity is.
fn bag(answer: &Answer) -> Vec<String> {
    bag_of(&answer.rows)
}

fn bag_of(rows: &[Vec<SqlValue>]) -> Vec<String> {
    let mut rows: Vec<String> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    SqlValue::Text(text) => text.clone(),
                    SqlValue::Int(i) => i.to_string(),
                    SqlValue::Null => "NULL".to_owned(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    rows.sort();
    rows
}

fn text(t: &str) -> Param {
    Param::Text(t.to_owned())
}

#[test]
fn the_opening_example_returns_properties_of_both_variables() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person)-[:performed]->(s IS song) \
         RETURN b.name AS band, p.name AS person, s.title AS song",
        &[text("b1")],
    );
    assert_eq!(answer.columns, ["band", "person", "song"]);
    assert_eq!(
        bag(&answer),
        [
            "band one|person one|song 1",
            "band one|person one|song 2",
            "band one|person two|song 1",
        ]
    );
    // Rebound, the same text finds the other band.
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person)-[:performed]->(s IS song) \
         RETURN p.name, s.title",
        &[text("b2")],
    );
    assert_eq!(answer.columns, ["name", "title"]);
    assert_eq!(bag(&answer), ["person three|song 3"]);
}

#[test]
fn a_three_hop_chain_binds_every_variable() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // band <- member -> performed song <- written by
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = 'b1')<-[:member_of]-(p)-[:performed]->(s)<-[:wrote]-(w IS person) \
         RETURN p._key AS p, s._key AS s, w._key AS w",
        &[],
    );
    assert_eq!(bag(&answer), ["p1|s1|p1", "p2|s1|p1"]);
}

#[test]
fn a_seed_in_the_middle_walks_both_ways() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // The key sits on the middle node: the band hop is walked from `p`
    // AGAINST the written arrow's reading order, and must still mean
    // p -member_of-> b.
    let answer = run(
        &db,
        "base MATCH (b IS band)<-[:member_of]-(p IS person WHERE p._key = 'p2')-[:performed]->(s) \
         RETURN b._key AS b, s._key AS s",
        &[],
    );
    assert_eq!(bag(&answer), ["b1|s1"]);
    // Anonymous elements are walked through too.
    let answer = run(
        &db,
        "base MATCH (:band)<-[:member_of]-(p IS person WHERE p._key = 'p1')-[:wrote]->(s) RETURN s._key",
        &[],
    );
    assert_eq!(bag(&answer), ["s1"]);
}

#[test]
fn comma_patterns_join_on_a_shared_variable() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // The second pattern starts from `p`, already bound: a bound seed.
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = 'b1')<-[:member_of]-(p IS person), (p)-[:wrote]->(s IS song) \
         RETURN p._key AS p, s._key AS s",
        &[],
    );
    assert_eq!(bag(&answer), ["p1|s1", "p2|s4"]);
    // Both ends of the second pattern are bound: ExpandInto keeps only the
    // songs `p1` both performed and wrote.
    let answer = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[:performed]->(s IS song), (p)-[:wrote]->(s) \
         RETURN s._key",
        &[],
    );
    assert_eq!(bag(&answer), ["s1"]);
    // No shared variable: a cross product.
    let answer = run(
        &db,
        "base MATCH (b IS band WHERE b._key = 'b2'), (s IS song WHERE s._key = $1) RETURN b._key, s._key",
        &[text("s4")],
    );
    assert_eq!(bag(&answer), ["b2|s4"]);
}

#[test]
fn label_alternation_on_edges_and_nodes() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Two edge types: `p1` performed s1 and s2 and wrote s1 -- s1 twice,
    // once per edge.
    let answer = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[e:performed|wrote]->(s) RETURN s._key",
        &[],
    );
    assert_eq!(bag(&answer), ["s1", "s1", "s2"]);
    // Two node labels on the far end: member_of reaches only bands.
    let answer = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[]->(x IS band|song) RETURN x._key",
        &[],
    );
    assert_eq!(bag(&answer), ["b1", "s1", "s1", "s2"]);
    let answer = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[]->(x IS song) RETURN x._key",
        &[],
    );
    assert_eq!(bag(&answer), ["s1", "s1", "s2"]);
}

#[test]
fn two_collections_sharing_a_key_are_two_nodes() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let answer = run(
        &db,
        "base MATCH (x IS band|venue WHERE x._key = 'b1') RETURN x.name",
        &[],
    );
    assert_eq!(bag(&answer), ["band one", "venue one"]);
    let answer = run(&db, "base MATCH (x IS venue WHERE x._key = 'b1') RETURN x.name", &[]);
    assert_eq!(bag(&answer), ["venue one"]);
    // The venue has no members: its key is the band's, its identity is not.
    let answer = run(
        &db,
        "base MATCH (x IS venue WHERE x._key = 'b1')<-[:member_of]-(p) RETURN p._key",
        &[],
    );
    assert!(answer.rows.is_empty(), "{:?}", answer.rows);
}

#[test]
fn a_repeated_node_variable_is_one_node() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // Back to the SAME person: one row. A fresh variable: every member.
    let same = run(
        &db,
        "base MATCH (a IS person WHERE a._key = 'p1')-[:member_of]->(b)<-[:member_of]-(a) RETURN b._key",
        &[],
    );
    assert_eq!(bag(&same), ["b1"]);
    let fresh = run(
        &db,
        "base MATCH (a IS person WHERE a._key = 'p1')-[:member_of]->(b)<-[:member_of]-(c) RETURN c._key",
        &[],
    );
    assert_eq!(bag(&fresh), ["p1", "p2"]);
    // `a = c` compares identity, and the answer is the repeated variable's.
    let equal = run(
        &db,
        "base MATCH (a IS person WHERE a._key = 'p1')-[:member_of]->(b)<-[:member_of]-(c) WHERE a = c RETURN c._key",
        &[],
    );
    assert_eq!(bag(&equal), ["p1"]);
    let unequal = run(
        &db,
        "base MATCH (a IS person WHERE a._key = 'p1')-[:member_of]->(b)<-[:member_of]-(c) WHERE a <> c RETURN c._key",
        &[],
    );
    assert_eq!(bag(&unequal), ["p2"]);
}

#[test]
fn a_repeated_edge_variable_is_one_edge() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // `e` binds in the first pattern; the second must walk the same edge.
    let answer = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[e:performed]->(s), \
         (q IS person WHERE q._key = 'p1')-[e]->(t) RETURN s._key, t._key",
        &[],
    );
    assert_eq!(bag(&answer), ["s1|s1", "s2|s2"]);
    // One name cannot be a node and an edge.
    let error = run_with(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[p]->(s) RETURN s._key",
        &[],
    )
    .unwrap_err();
    assert!(error.to_string().contains("`p`"), "{error}");
}

#[test]
fn a_missing_seed_gives_zero_rows_not_an_error() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = "base MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p) RETURN p.name";
    for key in [text("b9"), text("p1"), Param::Null] {
        let answer = run(&db, body, &[key.clone()]);
        assert!(answer.rows.is_empty(), "{key:?}: {:?}", answer.rows);
        assert_eq!(answer.columns, ["name"]);
    }
    // A disconnected target: the seed exists and nothing reaches it.
    let answer = run(
        &db,
        "base MATCH (s IS song WHERE s._key = 's4')<-[:performed]-(p) RETURN p._key",
        &[],
    );
    assert!(answer.rows.is_empty());
    // An unbound parameter is an error that names it.
    let error = run_with(&db, body, &[]).unwrap_err();
    assert!(matches!(error, SqlError::Parameter(_)), "{error}");
    assert!(error.to_string().contains("$1"), "{error}");
}

#[test]
fn inline_and_pattern_where_agree_on_the_answer_and_differ_in_work() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // A far-node predicate on the MIDDLE node: inline, it prunes `p2`
    // before the second hop walks its edges; after the pattern, it runs on
    // completed matches.
    let inline = "base MATCH (b IS band WHERE b._key = 'b1')<-[:member_of]-(p IS person WHERE p.age > 30)-[:performed]->(s) \
         RETURN s._key";
    let after = "base MATCH (b IS band WHERE b._key = 'b1')<-[:member_of]-(p IS person)-[:performed]->(s) \
         WHERE p.age > 30 RETURN s._key";
    assert_eq!(bag(&run(&db, inline, &[])), ["s1", "s2"]);
    assert_eq!(bag(&run(&db, inline, &[])), bag(&run(&db, after, &[])));
    let (inline_edges, after_edges) = (
        work(&db, inline, &[], "graph_edges"),
        work(&db, after, &[], "graph_edges"),
    );
    assert!(inline_edges < after_edges, "inline {inline_edges} vs after {after_edges}");
    // An inline EDGE predicate reads the edge's bag per hop.
    let inline = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[r:performed WHERE r.times > 2]->(s) RETURN s._key",
        &[],
    );
    let after = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[r:performed]->(s) WHERE r.times > 2 RETURN s._key",
        &[],
    );
    assert_eq!(bag(&inline), ["s1"]);
    assert_eq!(bag(&inline), bag(&after));
    // A far-node predicate reads the far row, one charged primary read per
    // edge that reaches it.
    let charged = "base MATCH (p IS person WHERE p._key = 'p1')-[:performed]->(s IS song WHERE s.year > 2001) RETURN s._key";
    assert_eq!(bag(&run(&db, charged, &[])), ["s2"]);
    assert_eq!(
        work(&db, charged, &[], "primary_reads"),
        2 + 1,
        "two far rows, one projected"
    );
}

#[test]
fn three_valued_logic_drops_unknown() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // p3 has no age: `p.age > 30` is unknown, and so is its negation.
    let members = |predicate: &str| {
        bag(&run(
            &db,
            &format!(
                "base MATCH (b IS band WHERE b._key = 'b2')<-[:member_of]-(p) WHERE {predicate} RETURN p._key"
            ),
            &[],
        ))
    };
    assert!(members("p.age > 30").is_empty());
    assert!(members("NOT (p.age > 30)").is_empty());
    assert!(members("p.age > 30 AND FALSE").is_empty());
    assert_eq!(members("p.age > 30 OR TRUE"), ["p3"]);
    assert_eq!(members("NOT (p.age > 30 AND FALSE)"), ["p3"]);
    assert!(members("p.age = NULL").is_empty());
    // A property no row holds reads as null, never as an error.
    assert!(members("p.nickname = 'x'").is_empty());
}

#[test]
fn an_indexed_label_scan_predicate_seeds_from_the_index() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let indexed = "base MATCH (p IS person) WHERE p.age = $1 RETURN p._key";
    assert_eq!(bag(&run(&db, indexed, &[Param::Int(25)])), ["p2"]);
    // The index answered the predicate: the one row read is the projection
    // of the one match, not a scan's refinement.
    assert!(work(&db, indexed, &[Param::Int(25)], "scalar_postings") >= 1);
    assert_eq!(work(&db, indexed, &[Param::Int(25)], "primary_reads"), 1);
    let answer = run(
        &db,
        "base MATCH (p IS person WHERE 30 < p.age)-[:member_of]->(b) RETURN p._key, b._key",
        &[],
    );
    assert_eq!(bag(&answer), ["p1|b1"]);
    // A null value satisfies no comparison: an empty seed stream.
    let answer = run(
        &db,
        "base MATCH (p IS person) WHERE p.age = $1 RETURN p._key",
        &[Param::Null],
    );
    assert!(answer.rows.is_empty());
    // A bare label scan with no predicate is admitted.
    let answer = run(&db, "base MATCH (s IS song) RETURN s._key", &[]);
    assert_eq!(bag(&answer), ["s1", "s2", "s3", "s4"]);
}

#[test]
fn an_unindexed_label_scan_predicate_is_refused_naming_the_index() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    for body in [
        "base MATCH (s IS song) WHERE s.year = 2001 RETURN s._key",
        "base MATCH (s IS song WHERE s.year > 2001) RETURN s._key",
    ] {
        let error = run_with(&db, body, &[]).unwrap_err();
        let message = error.to_string();
        assert!(matches!(error, SqlError::Unsupported(_)), "{message}");
        assert!(message.contains("`song`"), "{message}");
        assert!(message.contains("`year`"), "{message}");
        assert!(message.contains("CREATE INDEX"), "{message}");
    }
    // The same predicate on a node REACHED from a seed is a charged row
    // read, which the contract allows.
    let answer = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[:performed]->(s IS song) WHERE s.year = 2001 RETURN s._key",
        &[],
    );
    assert_eq!(bag(&answer), ["s1"]);
}

#[test]
fn the_graph_argument_names_a_context() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let body = |graph: &str| {
        format!("{graph} MATCH (p IS person WHERE p._key = 'p2')-[:performed]->(s) RETURN s._key")
    };
    assert_eq!(bag(&run(&db, &body("base"), &[])), ["s1"]);
    assert_eq!(bag(&run(&db, &body("ctx_a"), &[])), ["s2"]);
    // A context no edge was written in yet matches nothing, and says so.
    let unknown = run(&db, &body("ctx_z"), &[]);
    assert!(unknown.rows.is_empty());
    assert!(
        unknown.notices.iter().any(|n| n.contains("`ctx_z`")),
        "{:?}",
        unknown.notices
    );
    // An edge type no edge was written with yet matches nothing either.
    let answer = run(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p2')-[:produced]->(s) RETURN s._key",
        &[],
    );
    assert!(answer.rows.is_empty());
    assert!(answer.notices.iter().any(|n| n.contains("`produced`")), "{:?}", answer.notices);
}

#[test]
fn scope_errors_name_the_variable() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let error = run_with(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1') RETURN q.name",
        &[],
    )
    .unwrap_err();
    assert!(error.to_string().contains("`q`"), "{error}");
    let error = run_with(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1') WHERE x.age > 1 RETURN p.name",
        &[],
    )
    .unwrap_err();
    assert!(error.to_string().contains("`x`"), "{error}");
    // A node is not a column: the error says what to return instead.
    let error = run_with(&db, "base MATCH (p IS person WHERE p._key = 'p1') RETURN p", &[])
        .unwrap_err();
    assert!(error.to_string().contains("p._key"), "{error}");
    assert!(error.to_string().contains("ELEMENT_ID(p)"), "{error}");
    // An unknown label is an unknown collection, named.
    let error = run_with(&db, "base MATCH (p IS artist WHERE p._key = 'p1') RETURN p.name", &[])
        .unwrap_err();
    assert!(error.to_string().contains("`artist`"), "{error}");
    // A node is compared by identity only.
    let error = run_with(
        &db,
        "base MATCH (p IS person WHERE p._key = 'p1') WHERE p = 'p1' RETURN p.name",
        &[],
    )
    .unwrap_err();
    assert!(error.to_string().contains("`p`"), "{error}");
}

// ── the SQL surface of a GQL plan (M2-D) ───────────────────────────────────

const OPENING: &str = "base MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person)-[:performed]->(s IS song) \
     RETURN b.name AS band, p.name AS person, s.title AS song";

#[test]
fn rows_carry_no_owner_and_every_page_size_gives_the_one_shot_answer() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let one_shot = run(&db, OPENING, &[text("b1")]);
    assert_eq!(one_shot.rows.len(), 3);
    let prepared = prepare_sql(&db, &statement(OPENING), &[text("b1")]).unwrap();
    assert!(prepared.is_select() && !prepared.is_aggregate());
    assert_eq!(prepared.source_collection(), None, "a relation reads no one collection");
    for page_rows in [1, 2, 3, 64] {
        let mut paged = Vec::new();
        prepared
            .for_each_row(&db, page_rows, &mut |row| {
                assert_eq!(row.owner(), None);
                assert_eq!(row.id, EntityId::NO_OWNER);
                paged.push(row.values.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(bag_of(&paged), bag(&one_shot), "page_rows {page_rows}");
    }
    // `Database::sql` runs the same plan.
    match db.sql(&statement(OPENING), &[text("b1")]).unwrap() {
        SqlResult::Rows { columns, rows } => {
            assert_eq!(columns, one_shot.columns);
            assert_eq!(bag_of(&values(rows)), bag(&one_shot));
        }
        other => panic!("not rows: {other:?}"),
    }
    // A GQL plan is a relation: it has no single prepared engine query.
    let error = prepared.with_query(&db, &mut |_| Ok(())).unwrap_err();
    assert!(error.to_string().contains("GQL"), "{error}");
}

#[test]
fn a_bind_swaps_the_parameters_and_compiles_nothing() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let mut prepared = prepare_sql(&db, &statement(OPENING), &[text("b1")]).unwrap();
    assert!(prepared.rebindable(), "every $n of a GQL plan is read per execution");
    assert_eq!(prepared.rebind_refusal(), None);
    let rows = |prepared: &sekejap_lang::PreparedSql| match prepared.run(&db).unwrap() {
        SqlResult::Rows { rows, .. } => bag_of(&values(rows)),
        other => panic!("not rows: {other:?}"),
    };
    assert_eq!(rows(&prepared).len(), 3);
    prepared.bind(&db, &[text("b2")]).unwrap();
    assert_eq!(rows(&prepared), ["band two|person three|song 3"]);
    // A key no row holds: zero rows with the same columns, not an error.
    prepared.bind(&db, &[text("b9")]).unwrap();
    assert!(rows(&prepared).is_empty());
    assert_eq!(prepared.columns(), ["band", "person", "song"]);
    prepared.bind(&db, &[text("b1")]).unwrap();
    assert_eq!(rows(&prepared).len(), 3);
}

#[test]
fn columns_are_typed_from_the_binding_schema() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let prepared = prepare_sql(
        &db,
        &statement(
            "base MATCH (p IS person WHERE p._key = 'p1')-[r:performed]->(s IS song), \
             (x IS person|song WHERE x._key = 's1') \
             RETURN p.name, p.age, s.year, p._key AS k, r.times, x.age, p.age > 30 AS old, \
             1 AS one, 0.5 AS half, 'a' AS a, TRUE AS t, NULL AS n, p.nickname",
        ),
        &[],
    )
    .unwrap();
    let types: Vec<_> = (0..prepared.columns().len())
        .map(|at| prepared.column_type(at))
        .collect();
    assert_eq!(
        types,
        [
            Some("TEXT"),             // declared text
            Some("BIGINT"),           // declared int
            Some("BIGINT"),           // declared int, another collection
            Some("TEXT"),             // `_key`
            Some("TEXT"),             // an edge property is undeclared (Q8)
            Some("TEXT"),             // person.age is int, song has no age: mixed (Q8)
            Some("BOOLEAN"),          // a comparison
            Some("BIGINT"),
            Some("DOUBLE PRECISION"),
            Some("TEXT"),
            Some("BOOLEAN"),
            Some("TEXT"),             // the NULL literal
            Some("TEXT"),             // a property no collection declares
        ]
    );
    assert_eq!(prepared.column_type(13), None, "past the last column");
}

#[test]
fn a_declared_timestamp_column_is_typed_and_printed_as_the_sql_surface_prints_it() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("t.sekejap"), cfg()).unwrap();
    db.sql("CREATE TABLE gig (id TEXT PRIMARY KEY, at TIMESTAMPTZ, day DATE)", &[])
        .unwrap();
    db.sql(
        "INSERT INTO gig (id, at, day) VALUES ('g1', '2020-01-02T03:04:05Z', '2020-01-02')",
        &[],
    )
    .unwrap();
    db.commit().unwrap();
    let sql = match db.sql("SELECT at, day FROM gig WHERE _key = 'g1'", &[]).unwrap() {
        SqlResult::Rows { rows, .. } => rows[0].values.clone(),
        other => panic!("not rows: {other:?}"),
    };
    let body = "base MATCH (g IS gig WHERE g._key = 'g1') RETURN g.at AS at, g.day AS day";
    let prepared = prepare_sql(&db, &statement(body), &[]).unwrap();
    assert_eq!(prepared.column_type(0), Some("TIMESTAMPTZ"));
    assert_eq!(prepared.column_type(1), Some("DATE"));
    assert_eq!(run(&db, body, &[]).rows, [sql]);
}

#[test]
fn a_list_of_declared_timestamps_or_dates_is_typed_and_printed_as_the_scalar_column_is() {
    // A list of `TIMESTAMPTZ` is `TIMESTAMPTZ[]` and a list of `DATE` is
    // `DATE[]`, each item printed as the scalar column prints it -- never
    // `JSONB[]` holding the stored microseconds.
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("t.sekejap"), cfg()).unwrap();
    db.sql("CREATE TABLE gig (id TEXT PRIMARY KEY, at TIMESTAMPTZ, day DATE)", &[])
        .unwrap();
    db.sql(
        "INSERT INTO gig (id, at, day) VALUES ('g1', '2020-01-02T03:04:05Z', '2020-01-02')",
        &[],
    )
    .unwrap();
    db.commit().unwrap();
    let at = json!(["2020-01-02T03:04:05Z"]);
    let day = json!(["2020-01-02"]);
    for body in [
        // vertical
        "base MATCH (g IS gig WHERE g._key = 'g1') RETURN ARRAY_AGG(g.at) AS at, ARRAY_AGG(g.day) AS day",
        // a list literal
        "base MATCH (g IS gig WHERE g._key = 'g1') RETURN [g.at] AS at, [g.day] AS day",
        // horizontal, over a LET list
        "base MATCH (g IS gig WHERE g._key = 'g1') LET ats = [g.at], days = [g.day] \
         LET at = ARRAY_AGG(ats), day = ARRAY_AGG(days) RETURN at, day",
    ] {
        let prepared = prepare_sql(&db, &statement(body), &[]).unwrap();
        assert_eq!(prepared.column_type(0), Some("TIMESTAMPTZ[]"), "{body}");
        assert_eq!(prepared.column_type(1), Some("DATE[]"), "{body}");
        assert_eq!(
            run(&db, body, &[]).rows,
            [vec![SqlValue::Json(at.clone()), SqlValue::Json(day.clone())]],
            "{body}"
        );
    }
}

#[test]
fn parameters_are_typed_from_where_each_is_compared() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let types = |body: &str| prepare_sql(&db, &statement(body), &[]).unwrap().param_types();
    assert_eq!(types(OPENING), [Some("TEXT")], "a `_key` is text");
    assert_eq!(
        types("base MATCH (p IS person) WHERE p.age = $1 RETURN p._key"),
        [Some("BIGINT")]
    );
    // Reversed, and a second parameter compared with a text property.
    assert_eq!(
        types("base MATCH (p IS person WHERE $2 < p.age) WHERE p.name = $1 RETURN p._key"),
        [Some("TEXT"), Some("BIGINT")]
    );
    // An edge property is undeclared, so it decides nothing; a parameter
    // used as two types is refused by the parameter table (M3-D), with
    // PostgreSQL's `42P08`.
    assert_eq!(
        types("base MATCH (p IS person WHERE p._key = $1)-[r:performed WHERE r.times > $2]->(s IS song) RETURN s._key"),
        [Some("TEXT"), None]
    );
    let conflict = "base MATCH (p IS person WHERE p._key = $1)-[r:performed]->(s IS song WHERE s.year = $1) RETURN s._key";
    match prepare_sql(&db, &statement(conflict), &[]) {
        Err(SqlError::Coded { sqlstate, message }) => {
            assert_eq!(sqlstate, "42P08", "{message}");
            assert!(message.contains("TEXT") && message.contains("BIGINT"), "{message}");
        }
        other => panic!("{:?}", other.err()),
    }
}

