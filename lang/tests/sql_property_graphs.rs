//! Property graphs as VIEWS over the base graph (0.18.3, owner decision
//! 2026-09-28): the standard SQL/PGQ statements as Oracle 23ai and Spanner
//! write them, over tables in any schema.
//!
//! Three layers: `base` stores every edge once and always exists; an edge
//! table is the door edges are written through, its direction fixed ONCE; a
//! named property graph is a stored definition -- elements (a table, its
//! alias, its labels) -- that any number of graphs may share tables in.
//!
//! What is at risk, one test each:
//!
//! * aliases and labels over same-named tables in two schemas: `AS`, a
//!   shared `LABEL`, and the refusals Oracle makes (a duplicate element name,
//!   a shared label over different properties, a schema-qualified label)
//!   (`aliases_and_labels_span_schemas`);
//! * `base` is the default graph: `ALTER PROPERTY GRAPH base ADD EDGE TABLES`
//!   fixes an edge table's direction with no named graph, a label resolves to
//!   the one table of that name in any schema or is refused as ambiguous,
//!   `"schema.table"` picks one, `ADD LABEL` names tables in `base`, and
//!   dropping an edge table from `base` waits until its edges are deleted
//!   (`base_is_the_default_graph`);
//! * graphs share tables and change without touching an edge: a second
//!   graph names a fixed edge table without its ends, `ALTER ... ADD/DROP`,
//!   `ADD/DROP LABEL`, `CREATE OR REPLACE`, `DROP PROPERTY GRAPH`, and a named
//!   graph's unlabeled pattern covers its own tables only
//!   (`named_graphs_are_views_that_share_tables`);
//! * the definitions survive a reopen, and a graph recorded the pre-0.18.3
//!   way -- only as a name on its edge tables -- opens with that graph intact
//!   (`definitions_persist_and_an_older_graph_opens_intact`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn refused(db: &mut Database, sql: &str, says: &str) {
    match db.sql(sql, &[]) {
        Err(e) => assert!(e.to_string().contains(says), "`{sql}` should say `{says}`: {e}"),
        Ok(r) => panic!("`{sql}` should be refused, not {r:?}"),
    }
    let _ = db.sql("ROLLBACK", &[]);
}

/// The first column of every row, sorted.
fn firsts(db: &mut Database, sql: &str) -> Vec<String> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .into_iter()
                .map(|r| match &r.values[0] {
                    SqlValue::Text(t) => t.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            out.sort();
            out
        }
        other => panic!("`{sql}`: {other:?}"),
    }
}

/// People and the cities they live in, in two schemas with the same table
/// names.
fn world(db: &mut Database) {
    for sql in [
        "CREATE SCHEMA usa",
        "CREATE SCHEMA china",
        "CREATE TABLE person (_key TEXT PRIMARY KEY, name TEXT)",
        "CREATE TABLE usa.city (_key TEXT PRIMARY KEY, name TEXT)",
        "CREATE TABLE china.city (_key TEXT PRIMARY KEY, name TEXT)",
        "CREATE TABLE lives_in (p TEXT REFERENCES person, c TEXT REFERENCES usa.city)",
        "CREATE TABLE visited (p TEXT REFERENCES person, c TEXT REFERENCES china.city, stay INT)",
        "INSERT INTO person (_key, name) VALUES ('ayu', 'Ayu'), ('bayu', 'Bayu')",
        "INSERT INTO usa.city (_key, name) VALUES ('nyc', 'New York')",
        "INSERT INTO china.city (_key, name) VALUES ('bj', 'Beijing')",
        "COMMIT",
    ] {
        run(db, sql);
    }
}

#[test]
fn aliases_and_labels_span_schemas() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    world(&mut db);
    // Two tables named `city`: the same element name twice is Oracle's error.
    refused(
        &mut db,
        "CREATE PROPERTY GRAPH bad VERTEX TABLES (usa.city, china.city)",
        "AS",
    );
    run(
        &mut db,
        "CREATE PROPERTY GRAPH travel
           VERTEX TABLES (person, usa.city AS usa_city DEFAULT LABEL LABEL city, china.city AS china_city DEFAULT LABEL LABEL city)
           EDGE TABLES (
             lives_in SOURCE KEY (p) REFERENCES person (_key) DESTINATION KEY (c) REFERENCES usa.city (_key),
             visited  SOURCE KEY (p) REFERENCES person (_key) DESTINATION KEY (c) REFERENCES china.city (_key))",
    );
    run(&mut db, "INSERT INTO lives_in (p, c) VALUES ('ayu', 'nyc')");
    run(&mut db, "INSERT INTO visited (p, c, stay) VALUES ('ayu', 'bj', 3), ('bayu', 'bj', 1)");
    run(&mut db, "COMMIT");
    // The shared label reaches both schemas; each alias reaches one.
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (travel MATCH (c:city) RETURN c.name AS name)"),
        ["Beijing", "New York"]
    );
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (travel MATCH (c IS china_city) RETURN c.name AS name)"),
        ["Beijing"]
    );
    assert_eq!(
        firsts(
            &mut db,
            "SELECT * FROM GRAPH_TABLE (travel MATCH (p:person)-[:visited|lives_in]->(c:city) RETURN p.name || '>' || c.name AS trip)"
        ),
        ["Ayu>Beijing", "Ayu>New York", "Bayu>Beijing"]
    );
    // A label is a plain name: the schema belongs in the definition.
    refused(
        &mut db,
        "SELECT * FROM GRAPH_TABLE (travel MATCH (a)-[r:usa.lives_in]->(b) RETURN a.name AS n)",
        "schema",
    );
    // A shared label must expose the same properties on every table.
    run(&mut db, "CREATE TABLE china.town (_key TEXT PRIMARY KEY, name TEXT, province TEXT)");
    refused(
        &mut db,
        "ALTER PROPERTY GRAPH travel ADD VERTEX TABLES (china.town AS town LABEL city)",
        "province",
    );
}

#[test]
fn base_is_the_default_graph() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    world(&mut db);
    // An edge table needs its direction before it takes a row...
    refused(&mut db, "INSERT INTO visited (p, c) VALUES ('ayu', 'bj')", "PROPERTY GRAPH");
    // ... and `base` can fix it, with no named graph at all.
    run(
        &mut db,
        "ALTER PROPERTY GRAPH base ADD EDGE TABLES (
           visited SOURCE KEY (p) REFERENCES person (_key) DESTINATION KEY (c) REFERENCES china.city (_key))",
    );
    run(&mut db, "INSERT INTO visited (p, c, stay) VALUES ('ayu', 'bj', 3)");
    run(&mut db, "COMMIT");
    assert_eq!(
        firsts(
            &mut db,
            "SELECT * FROM GRAPH_TABLE (base MATCH (p:person)-[v:visited]->(c) RETURN p.name AS name)"
        ),
        ["Ayu"]
    );
    // `city` is two tables in `base`: refused, naming both.
    refused(
        &mut db,
        "SELECT * FROM GRAPH_TABLE (base MATCH (c:city) RETURN c.name AS name)",
        "china.city",
    );
    // The qualified spelling picks one, in `base` only.
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (base MATCH (c:\"china.city\") RETURN c.name AS name)"),
        ["Beijing"]
    );
    // A label of `base` names a table there.
    run(&mut db, "ALTER PROPERTY GRAPH base ALTER VERTEX TABLE china.city ADD LABEL china_city");
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (base MATCH (c:china_city) RETURN c.name AS name)"),
        ["Beijing"]
    );
    // Every table is in `base` already.
    run(&mut db, "ALTER PROPERTY GRAPH base ADD VERTEX TABLES (person)");
    // `base` always exists and always holds everything.
    refused(&mut db, "DROP PROPERTY GRAPH base", "base");
    refused(&mut db, "CREATE OR REPLACE PROPERTY GRAPH base VERTEX TABLES (person)", "base");
    refused(&mut db, "CREATE PROPERTY GRAPH base VERTEX TABLES (person)", "base");
    // Taking an edge table's direction back would orphan its edges.
    refused(&mut db, "ALTER PROPERTY GRAPH base DROP EDGE TABLES (visited)", "DELETE FROM visited");
    run(&mut db, "DELETE FROM visited WHERE p = 'ayu'");
    run(&mut db, "COMMIT");
    run(&mut db, "ALTER PROPERTY GRAPH base DROP EDGE TABLES (visited)");
    refused(&mut db, "INSERT INTO visited (p, c) VALUES ('ayu', 'bj')", "PROPERTY GRAPH");
}

#[test]
fn named_graphs_are_views_that_share_tables() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    world(&mut db);
    run(
        &mut db,
        "CREATE PROPERTY GRAPH home
           VERTEX TABLES (person, usa.city AS city)
           EDGE TABLES (lives_in SOURCE KEY (p) REFERENCES person (_key) DESTINATION KEY (c) REFERENCES usa.city (_key))",
    );
    run(&mut db, "INSERT INTO lives_in (p, c) VALUES ('ayu', 'nyc')");
    run(&mut db, "COMMIT");
    // A second graph names the fixed edge table without its ends ...
    run(
        &mut db,
        "CREATE PROPERTY GRAPH people VERTEX TABLES (person, usa.city AS city) EDGE TABLES (lives_in)",
    );
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (people MATCH (p)-[:lives_in]->(c) RETURN p.name AS name)"),
        ["Ayu"]
    );
    // ... and cannot turn it round.
    refused(
        &mut db,
        "CREATE PROPERTY GRAPH wrong VERTEX TABLES (person, usa.city AS city)
           EDGE TABLES (lives_in SOURCE KEY (c) REFERENCES usa.city (_key) DESTINATION KEY (p) REFERENCES person (_key))",
        "person",
    );
    // A named graph's unlabeled pattern covers its own tables only.
    run(&mut db, "CREATE PROPERTY GRAPH only_people VERTEX TABLES (person)");
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (only_people MATCH (n) RETURN n.name AS name)"),
        ["Ayu", "Bayu"]
    );
    // Removing an edge table from a named graph deletes nothing.
    run(&mut db, "ALTER PROPERTY GRAPH people DROP EDGE TABLES (lives_in)");
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (home MATCH (p)-[:lives_in]->(c) RETURN p.name AS name)"),
        ["Ayu"]
    );
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (base MATCH (p)-[:lives_in]->(c) RETURN p.name AS name)"),
        ["Ayu"]
    );
    refused(
        &mut db,
        "SELECT * FROM GRAPH_TABLE (people MATCH (p)-[:lives_in]->(c) RETURN p.name AS name)",
        "lives_in",
    );
    run(&mut db, "ALTER PROPERTY GRAPH people ADD EDGE TABLES (lives_in)");
    // Labels come and go; an element keeps at least one.
    run(&mut db, "ALTER PROPERTY GRAPH people ALTER VERTEX TABLE city ADD LABEL place");
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (people MATCH (c:place) RETURN c.name AS name)"),
        ["New York"]
    );
    run(&mut db, "ALTER PROPERTY GRAPH people ALTER VERTEX TABLE city DROP LABEL city");
    refused(&mut db, "ALTER PROPERTY GRAPH people ALTER VERTEX TABLE city DROP LABEL place", "label");
    // A vertex table an edge table of the graph still reaches stays.
    refused(&mut db, "ALTER PROPERTY GRAPH people DROP VERTEX TABLES (person)", "lives_in");
    // CREATE OR REPLACE rewrites the definition, never an edge.
    run(&mut db, "CREATE OR REPLACE PROPERTY GRAPH people VERTEX TABLES (person)");
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (people MATCH (n) RETURN n.name AS name)"),
        ["Ayu", "Bayu"]
    );
    run(&mut db, "DROP PROPERTY GRAPH home");
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (base MATCH (p)-[:lives_in]->(c) RETURN p.name AS name)"),
        ["Ayu"]
    );
    refused(&mut db, "CREATE PROPERTY GRAPH people VERTEX TABLES (person)", "already exists");
}

#[test]
fn definitions_persist_and_an_older_graph_opens_intact() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("g.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        world(&mut db);
        run(
            &mut db,
            "CREATE PROPERTY GRAPH travel
               VERTEX TABLES (person, usa.city AS usa_city LABEL city, china.city AS china_city LABEL city)
               EDGE TABLES (visited SOURCE KEY (p) REFERENCES person (_key) DESTINATION KEY (c) REFERENCES china.city (_key))",
        );
        // The pre-0.18.3 record of a graph: only its name on the edge table,
        // written through the engine the way 0.18.0-0.18.2 wrote it.
        let lives_in = db.collection("lives_in").unwrap().unwrap();
        db.bind_edge_table(lives_in, "p", "c", "lives_in", "older").unwrap();
        run(&mut db, "INSERT INTO lives_in (p, c) VALUES ('ayu', 'nyc')");
        run(&mut db, "INSERT INTO visited (p, c) VALUES ('bayu', 'bj')");
        run(&mut db, "COMMIT");
    }
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (travel MATCH (p)-[:visited]->(c:city) RETURN c.name AS name)"),
        ["Beijing"]
    );
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (older MATCH (p:person)-[:lives_in]->(c) RETURN p.name AS name)"),
        ["Ayu"]
    );
    // The older graph is a graph like any other from here on.
    run(&mut db, "ALTER PROPERTY GRAPH older ADD VERTEX TABLES (china.city AS china_city)");
    run(&mut db, "DROP PROPERTY GRAPH older");
    assert_eq!(
        firsts(&mut db, "SELECT * FROM GRAPH_TABLE (base MATCH (p)-[:lives_in]->(c) RETURN p.name AS name)"),
        ["Ayu"]
    );
}
