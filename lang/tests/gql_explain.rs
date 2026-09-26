//! `EXPLAIN` of a GQL statement (`docs/lang/GQL_PROFILE_DESIGN.md` §8, M2-D).
//!
//! What is at risk, and the test that pins it:
//!
//! * every operator is printed with how its seed is accessed -- a key
//!   lookup, an index by name, a node already bound, a SCAN -- and every
//!   predicate with WHERE it runs: per edge, per far node, right after the
//!   seed, or after the pattern (`every_operator_*`, `each_seed_*`);
//! * the stage's slot schema: name, type, nullability, provenance;
//! * the budget resources each operator can be stopped by, the rebind
//!   status, and after the run the rows and the work it charged;
//! * it is built from the PLAN, never from the text: two spellings of one
//!   statement explain identically (`two_spellings_*`);
//! * a name unknown at prepare is shown as looked up again per execution
//!   (`names_unknown_at_prepare_*`);
//! * an `EXISTS { ... }` is an `ExistsApply` with its body's steps under it,
//!   in its filter form or its mark form, the mark a hidden `BOOLEAN` slot,
//!   and the body's own variables listed as local to it (M5-C,
//!   `an_exists_is_*`).
//!
//! Workload names are invented: bands `b1` `b2`, people `p1` `p2`, songs
//! `s1` `s2`.

use sekejap_core::collections::{Database, GraphContextId};
use sekejap_core::Kind;
use sekejap_lang::{explain_sql, Param, SqlDatabase, SqlResult};
use serde_json::json;
use tempfile::TempDir;

mod common;
use common::cfg;

/// ```text
/// band   b1, b2            person p1 age 41, p2 age 25 (age indexed)
/// song   s1 year 2001, s2 year 2002
/// member_of  p1->b1  p2->b1
/// performed  p1->s1 {times 3}  p1->s2 {times 1}  p2->s1 {times 5}
/// ```
fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    let text = |name: &str| (name.to_owned(), Kind::Text);
    let int = |name: &str| (name.to_owned(), Kind::Int);
    let band = db
        .create_collection("band", vec![text("name")], Default::default())
        .unwrap();
    let person = db
        .create_collection("person", vec![text("name"), int("age")], Default::default())
        .unwrap();
    let song = db
        .create_collection("song", vec![text("title"), int("year")], Default::default())
        .unwrap();
    let b1 = db.put(band, "b1", &json!({"name": "band one"})).unwrap();
    db.put(band, "b2", &json!({"name": "band two"})).unwrap();
    let p1 = db
        .put(person, "p1", &json!({"name": "person one", "age": 41}))
        .unwrap();
    let p2 = db
        .put(person, "p2", &json!({"name": "person two", "age": 25}))
        .unwrap();
    let s1 = db
        .put(song, "s1", &json!({"title": "song 1", "year": 2001}))
        .unwrap();
    let s2 = db
        .put(song, "s2", &json!({"title": "song 2", "year": 2002}))
        .unwrap();
    let age = db.create_scalar_index(person, "person_age", "age", false).unwrap();
    while !db.build_index_step(age, 64).unwrap() {
        db.commit().unwrap();
    }
    db.enable_graph().unwrap();
    let member_of = db.create_edge_type("member_of").unwrap();
    let performed = db.create_edge_type("performed").unwrap();
    let base = GraphContextId::BASE;
    for (source, edge_type, destination, bag) in [
        (p1, member_of, b1, json!({})),
        (p2, member_of, b1, json!({})),
        (p1, performed, s1, json!({"times": 3})),
        (p1, performed, s2, json!({"times": 1})),
        (p2, performed, s1, json!({"times": 5})),
    ] {
        db.create_edge(base, source, edge_type, destination, &bag)
            .unwrap();
    }
    db.commit().unwrap();
    db
}

fn statement(body: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE ({body})")
}

fn explain(db: &mut Database, body: &str, params: &[Param]) -> String {
    match db
        .sql(&format!("EXPLAIN {}", statement(body)), params)
        .unwrap_or_else(|error| panic!("EXPLAIN `{body}`: {error}"))
    {
        SqlResult::Explain(text) => text,
        other => panic!("not an explanation: {other:?}"),
    }
}

fn has(text: &str, line: &str) {
    assert!(
        text.lines().any(|l| l.trim() == line),
        "no line `{line}` in:\n{text}"
    );
}

fn contains(text: &str, part: &str) {
    assert!(text.contains(part), "no `{part}` in:\n{text}");
}

const PLACED: &str = "base MATCH (b IS band WHERE b._key = $1)<-[:member_of]-(p IS person WHERE p.age > 30)\
     -[r:performed WHERE r.times > 2]->(s IS song) WHERE s.year >= 2001 \
     RETURN p.name AS person, s.title AS song";

#[test]
fn every_operator_is_printed_with_its_seed_and_where_each_predicate_runs() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let text = explain(&mut db, PLACED, &[Param::Text("b1".into())]);
    has(&text, "GQL plan over graph `base` (the base graph), 1 stage");
    has(&text, "stage 1:");
    // The slot schema.
    has(&text, "slots:");
    has(&text, "0 b: node of band, never null, bound at pattern 1 position 0");
    has(&text, "1 p: node of person, never null, bound at pattern 1 position 2");
    has(&text, "2 r: edge of performed, never null, bound at pattern 1 position 3");
    has(&text, "3 s: node of song, never null, bound at pattern 1 position 4");
    // The operators, first to last, each with its seed and its charges.
    has(&text, "operators, first to last:");
    has(
        &text,
        "1. Seed b: key lookup of $1 in band -- charges key_postings, binding_rows",
    );
    has(
        &text,
        "2. Expand b <-[:member_of]- p: incoming edges, p a new node of person -- charges graph_edges, binding_rows",
    );
    has(
        &text,
        "far filter, per far node: (p.age > 30) -- charges primary_reads",
    );
    has(
        &text,
        "3. Expand p -[r:performed]-> s: outgoing edges, s a new node of song -- charges graph_edges, binding_rows",
    );
    has(
        &text,
        "edge filter, per edge: (r.times > 2) -- an outgoing edge carries its bag; an incoming one charges graph_edges",
    );
    has(
        &text,
        "4. Filter after the pattern: (s.year >= 2001) -- charges primary_reads, graph_edges",
    );
    has(
        &text,
        "5. Project person := p.name, song := s.title -- charges primary_reads, graph_edges",
    );
    has(&text, "columns: person TEXT, song TEXT");
    contains(&text, "budget: every operator stops at the deadline and at a cancel");
    contains(&text, "rebind: yes");
    // It ran: the answer and its work.
    has(&text, "rows: 1 in 1 page(s)");
    let work = text
        .lines()
        .find(|line| line.starts_with("work: "))
        .unwrap_or_else(|| panic!("no work line:\n{text}"));
    for counter in ["key_postings=1", "binding_rows=", "graph_edges=", "primary_reads="] {
        assert!(work.contains(counter), "{counter} in {work}");
    }
}

#[test]
fn each_seed_is_named_by_how_it_is_accessed() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let text = explain(
        &mut db,
        "base MATCH (p IS person) WHERE p.age = $1 RETURN p._key AS k",
        &[Param::Int(25)],
    );
    has(
        &text,
        "1. Seed p: index `person_age` on person (age = $1) -- charges scalar_postings, candidates, binding_rows",
    );
    has(&text, "rows: 1 in 1 page(s)");
    let text = explain(&mut db, "base MATCH (s IS song) RETURN s._key AS k", &[]);
    has(
        &text,
        "1. Seed s: SCAN of song -- charges candidates, binding_rows",
    );
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[:performed]->(s IS song), (p)-[:member_of]->(b) \
         RETURN s._key AS s, b._key AS b",
        &[],
    );
    has(&text, "1. Seed p: key lookup of 'p1' in person -- charges key_postings, binding_rows");
    has(
        &text,
        "3. Seed p: the node already bound in p -- charges binding_rows",
    );
    has(
        &text,
        "4. Expand p -[:member_of]-> b: outgoing edges, b a new node of any collection -- charges graph_edges, binding_rows",
    );
    // A repeated far node is an ExpandInto.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[:performed]->(s IS song), (p)-[:performed]->(s) RETURN s._key AS s",
        &[],
    );
    has(
        &text,
        "4. Expand p -[:performed]-> s: outgoing edges, into s, already bound (ExpandInto) -- charges graph_edges, binding_rows",
    );
    // A seed's own residual predicate runs right after the seed.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = 'p1' AND p.age > 30) RETURN p.name AS name",
        &[],
    );
    has(
        &text,
        "2. Filter right after the seed: (p.age > 30) -- charges primary_reads, graph_edges",
    );
}

#[test]
fn two_spellings_of_one_statement_explain_identically() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let colon = explain(
        &mut db,
        "base MATCH (b:band WHERE b._key = 'b1')<-[:member_of]-(p:person) RETURN p.name AS n",
        &[],
    );
    let is = explain(
        &mut db,
        "base   MATCH (B IS band WHERE B._key='b1') <-[ IS member_of]- (P IS person) RETURN P.name AS n",
        &[],
    );
    assert_eq!(colon, is);
    has(
        &colon,
        "statement: GRAPH_TABLE (base MATCH (b:band WHERE (b._key = 'b1'))<-[:member_of]-(p:person) RETURN p.name AS n)",
    );
    // `explain_sql` is the same plan, run the same way.
    let unkeyworded = explain_sql(
        &db,
        &statement("base MATCH (b:band WHERE b._key = 'b1')<-[:member_of]-(p:person) RETURN p.name AS n"),
        &[],
    )
    .unwrap();
    assert_eq!(unkeyworded, colon);
}

#[test]
fn names_unknown_at_prepare_are_shown_as_looked_up_again() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let text = explain(
        &mut db,
        "ctx_z MATCH (p IS person WHERE p._key = 'p1')-[:produced]->(s) RETURN s._key AS s",
        &[],
    );
    has(
        &text,
        "GQL plan over graph `ctx_z` (a named context no edge has used yet: looked up again each time the statement runs), 1 stage",
    );
    has(
        &text,
        "2. Expand p -[:produced]-> s: outgoing edges, s a new node of any collection -- charges graph_edges, binding_rows",
    );
    contains(&text, "edge type `produced`: no edge has used it yet");
    has(&text, "rows: 0 in 1 page(s)");
    contains(&text, "notice: graph context `ctx_z` has no edge yet");
}

#[test]
fn a_slot_line_names_the_type_the_slot_holds() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    // A group variable is a list of edges of its label, and never null: an
    // execution writes a list, empty for zero iterations.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[e:performed]->{1,2}(s IS song) \
         LET n = COUNT(e) RETURN s._key AS s, n",
        &[],
    );
    contains(&text, "e: list of edges of performed, never null, bound at pattern 1");
    // A labelled edge of a path pattern is an edge of its label.
    let text = explain(
        &mut db,
        "base MATCH q = (p IS person WHERE p._key = 'p1')-[r:performed]->(s IS song) \
         RETURN s._key AS s, PATH_LENGTH(q) AS n",
        &[],
    );
    contains(&text, "r: edge of performed, never null, bound at pattern 1");
    // Under an OPTIONAL MATCH it may be null.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = 'p1') \
         OPTIONAL MATCH (p)-[e:performed]->{1,2}(s IS song) \
         LET n = COUNT(e) RETURN p._key AS p, n",
        &[],
    );
    contains(&text, "e: list of edges of performed, nullable, bound at pattern 2");
}

#[test]
fn a_value_slot_is_nullable_only_when_a_row_may_hold_null_there() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    // A COUNT is never null, horizontal (over a list that is never null) or
    // vertical, and neither is a column that carries one through NEXT; a
    // SUM, a MIN or a property may be.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = 'p1')-[e:performed]->{1,2}(s IS song) \
         LET n = COUNT(e), t = SUM(e.times) \
         RETURN s, COUNT(p) AS c, MIN(n) AS low GROUP BY s \
         NEXT RETURN s._key AS k, c, low",
        &[],
    );
    contains(&text, "n: BIGINT, never null, bound at a LET of stage 1");
    contains(&text, "t: DOUBLE PRECISION, nullable, bound at a LET of stage 1");
    contains(&text, "c: BIGINT, never null, returned by stage 1");
    contains(&text, "low: BIGINT, nullable, returned by stage 1");
    // Under an OPTIONAL MATCH, the COUNT of a list that may be null may be
    // null too.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = 'p1') \
         OPTIONAL MATCH (p)-[e:performed]->{1,2}(s IS song) \
         LET n = COUNT(e) RETURN p._key AS p, n",
        &[],
    );
    contains(&text, "n: BIGINT, nullable, bound at a LET of stage 1");
}

#[test]
fn an_exists_is_an_exists_apply_with_its_steps_under_it() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    // The filter form: a top-level conjunct, here negated.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = $1) \
         FILTER NOT EXISTS { MATCH (p)-[:performed]->(s IS song WHERE s.year > 2001) } \
         RETURN p.name AS n",
        &[Param::Text("p2".into())],
    );
    has(&text, "0 p: node of person, never null, bound at pattern 1 position 0");
    has(
        &text,
        "1 s: node of song, never null, bound at pattern 2 position 2, local to an EXISTS body",
    );
    has(
        &text,
        "2. ExistsApply NOT EXISTS: per input row, steps 2.1-2.2 run from it until their first row; the row is kept when they give none -- charges nothing itself; the steps are rebuilt for each input row",
    );
    has(&text, "2.1. Seed p: the node already bound in p -- charges binding_rows");
    has(
        &text,
        "2.2. Expand p -[:performed]-> s: outgoing edges, s a new node of song -- charges graph_edges, binding_rows",
    );
    has(&text, "far filter, per far node: (s.year > 2001) -- charges primary_reads");
    has(&text, "3. Project n := p.name -- charges primary_reads, graph_edges");
    has(&text, "rows: 1 in 1 page(s)");
    // Not negated.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = $1) \
         FILTER EXISTS { (p)-[:performed]->(:song) } RETURN p.name AS n",
        &[Param::Text("p2".into())],
    );
    has(
        &text,
        "2. ExistsApply EXISTS: per input row, steps 2.1-2.2 run from it until their first row; the row is kept when they give one -- charges nothing itself; the steps are rebuilt for each input row",
    );
    // The mark form: a RETURN item reads the hidden slot the apply writes.
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = $1) \
         RETURN p.name AS n, EXISTS { (p)-[:member_of]->(:band) } AS member",
        &[Param::Text("p2".into())],
    );
    has(&text, "1 #1: BOOLEAN, never null, bound at an EXISTS of stage 1");
    has(
        &text,
        "2. ExistsApply EXISTS into hidden slot #1: per input row, steps 2.1-2.2 run from it until their first row; TRUE when they give one, FALSE when none, and every row is kept -- charges nothing itself; the steps are rebuilt for each input row",
    );
    has(
        &text,
        "3. Project n := p.name, member := #1 -- charges primary_reads, graph_edges",
    );
    has(&text, "columns: n TEXT, member BOOLEAN");
}

/// M5-E: a union in a first part is one `Union` over its branches, each
/// printed with its own slots and steps; `UNION` is the existing `Distinct`
/// after it.
#[test]
fn a_union_explains_each_branch_with_its_slots_and_steps() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let text = explain(
        &mut db,
        "base MATCH (s IS song WHERE s._key = 's1') RETURN s.title AS title \
         UNION MATCH (p IS person WHERE p._key = 'p1')-[:performed]->(s IS song) \
         RETURN s.title AS title",
        &[],
    );
    has(&text, "GQL plan over graph `base` (the base graph), 1 stage");
    has(&text, "0 title: TEXT, nullable, returned by stage 1");
    has(&text, "branch 1 slots:");
    has(&text, "branch 2 slots:");
    has(&text, "0 s: node of song, never null, bound at pattern 1 position 0");
    has(&text, "0 p: node of person, never null, bound at pattern 1 position 0");
    has(
        &text,
        "1. Union DISTINCT of 2 branches, then Distinct: branch order, then row order -- branch 1 is steps 1.1-1.2, branch 2 is steps 1.3-1.5; charges nothing of its own",
    );
    has(&text, "1.1. Seed s: key lookup of 's1' in song -- charges key_postings, binding_rows");
    has(&text, "1.2. Project title := s.title -- charges primary_reads, graph_edges");
    has(&text, "1.3. Seed p: key lookup of 'p1' in person -- charges key_postings, binding_rows");
    has(&text, "1.5. Project title := s.title -- charges primary_reads, graph_edges");
    has(&text, "2. Distinct -- holds sort_bytes");
    has(&text, "columns: title TEXT");
    has(&text, "rows: 2 in 1 page(s)");
    // UNION ALL has no Distinct.
    let all = explain(
        &mut db,
        "base MATCH (s IS song WHERE s._key = 's1') RETURN s.title AS title \
         UNION ALL MATCH (s IS song WHERE s._key = 's2') RETURN s.title AS title",
        &[],
    );
    has(
        &all,
        "1. Union ALL of 2 branches: branch order, then row order -- branch 1 is steps 1.1-1.2, branch 2 is steps 1.3-1.4; charges nothing of its own",
    );
    assert!(!all.contains("Distinct"), "{all}");
}

/// M5-E, Q25: a union after `NEXT` reads the incoming table once and holds
/// it for its branches, each of which runs over the whole of it.
#[test]
fn a_union_after_next_is_buffered_once_for_its_branches() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let text = explain(
        &mut db,
        "base MATCH (p IS person WHERE p._key = 'p1') RETURN p \
         NEXT MATCH (p)-[:performed]->(s IS song) RETURN s.title AS title \
         UNION ALL MATCH (p)-[:member_of]->(b IS band) RETURN b.name AS title",
        &[],
    );
    has(&text, "GQL plan over graph `base` (the base graph), 2 stages");
    has(&text, "NEXT: stage 2 reads the rows stage 1 returned, and nothing else of them");
    has(
        &text,
        "3. Buffered Union ALL of 2 branches: the rows stage 1 returned, held once and read by each branch from its start; branch order, then row order -- branch 1 is steps 3.1-3.3, branch 2 is steps 3.4-3.6; holds sort_bytes",
    );
    // Each branch's slots start with the incoming columns.
    has(&text, "0 p: node of person, never null, returned by stage 1");
    has(&text, "3.1. Seed p: the node already bound in p -- charges binding_rows");
    has(
        &text,
        "3.5. Expand p -[:member_of]-> b: outgoing edges, b a new node of band -- charges graph_edges, binding_rows",
    );
    has(&text, "3.6. Project title := b.name -- charges primary_reads, graph_edges");
    has(&text, "rows: 3 in 1 page(s)");
}
