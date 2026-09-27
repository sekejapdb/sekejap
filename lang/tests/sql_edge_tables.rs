//! Edge tables in SQL (`docs/core/EDGE_TABLES.md`): the PostgreSQL 19,
//! Oracle 23ai and Spanner way of writing a property graph's edges --
//! `CREATE TABLE` with `REFERENCES`, `CREATE PROPERTY GRAPH ... EDGE TABLES`,
//! then plain `INSERT`, `UPDATE`, `DELETE` and `SELECT` -- over native edges.
//!
//! What is at risk, one test each:
//!
//! * the music example end to end: artists, songs, four edge tables, and a
//!   GQL question over them (`the_music_graph_is_written_and_read_in_sql`);
//! * a plain INSERT never overwrites: a taken key is 23505, a missing end
//!   23503, a missing end column 23502 (`insert_raises_what_postgresql_raises`);
//! * the key decides how many edges a pair may have -- Jakarta twice, then
//!   Bandung (`the_key_decides_how_many_edges_a_pair_may_have`);
//! * ON CONFLICT DO NOTHING and DO UPDATE SET c = EXCLUDED.c
//!   (`on_conflict_does_nothing_or_updates`);
//! * UPDATE and DELETE by a WHERE that names an end, and refused without one
//!   (`update_and_delete_name_an_end`);
//! * SELECT from an edge table: an end in the WHERE, ORDER BY, LIMIT, DATE
//!   properties read back as dates, `$n` parameters
//!   (`select_reads_one_ends_edges`);
//! * what is refused, by name (`what_is_not_mapped_is_refused_by_name`);
//! * DROP PROPERTY GRAPH forgets the graph and keeps every edge
//!   (`drop_property_graph_keeps_the_edges`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{Param, SqlDatabase, SqlError, SqlResult, SqlValue};
use tempfile::TempDir;

fn db(dir: &TempDir) -> Database {
    Database::create(
        dir.path().join("music.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap()
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<SqlValue>> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn text(v: &SqlValue) -> String {
    match v {
        SqlValue::Text(t) => t.clone(),
        SqlValue::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

fn sorted_texts(rows: Vec<Vec<SqlValue>>) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| r.iter().map(text).collect::<Vec<_>>().join("|"))
        .collect();
    out.sort();
    out
}

fn sqlstate(result: Result<SqlResult, SqlError>) -> &'static str {
    match result {
        Err(SqlError::Coded { sqlstate, .. }) => sqlstate,
        other => panic!("a coded error, not {other:?}"),
    }
}

fn refused(result: Result<SqlResult, SqlError>, says: &str) {
    match result {
        Err(e) => assert!(e.to_string().contains(says), "`{e}` does not say `{says}`"),
        Ok(r) => panic!("refused, not {r:?}"),
    }
}

/// Artists and songs, and the four edge tables of the music example,
/// declared in one property graph.
fn music(db: &mut Database) {
    for ddl in [
        "CREATE TABLE artist (_key TEXT PRIMARY KEY, name TEXT, kind TEXT)",
        "CREATE TABLE song (_key TEXT PRIMARY KEY, title TEXT)",
        "CREATE TABLE wrote (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song, PRIMARY KEY (artist_id, song_id))",
        "CREATE TABLE member_of (member_id TEXT REFERENCES artist, group_id TEXT REFERENCES artist, since INT, PRIMARY KEY (member_id, group_id))",
        "CREATE TABLE performed (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song (_key), performed_on DATE, venue TEXT, PRIMARY KEY (artist_id, song_id, performed_on, venue))",
        "CREATE TABLE belongs_to (song_id TEXT REFERENCES song, artist_id TEXT REFERENCES artist, PRIMARY KEY (song_id))",
        "INSERT INTO artist (_key, name, kind) VALUES ('dhani', 'Dhani', 'individual'), ('andra', 'Andra', 'individual'), ('dewa', 'Dewa', 'band')",
        "INSERT INTO song (_key, title) VALUES ('kirana', 'Kirana'), ('separuh', 'Separuh Nafas')",
        "CREATE PROPERTY GRAPH music
           VERTEX TABLES (artist, song)
           EDGE TABLES (
             wrote      SOURCE KEY (artist_id) REFERENCES artist (_key) DESTINATION KEY (song_id)   REFERENCES song (_key),
             member_of  SOURCE KEY (member_id) REFERENCES artist (_key) DESTINATION KEY (group_id)  REFERENCES artist (_key),
             performed  SOURCE KEY (artist_id) REFERENCES artist (_key) DESTINATION KEY (song_id)   REFERENCES song (_key),
             belongs_to SOURCE KEY (song_id)   REFERENCES song (_key)   DESTINATION KEY (artist_id) REFERENCES artist (_key))",
    ] {
        run(db, ddl);
    }
}

#[test]
fn the_music_graph_is_written_and_read_in_sql() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    music(&mut db);
    for dml in [
        "INSERT INTO wrote VALUES ('dhani', 'kirana'), ('andra', 'kirana')",
        "INSERT INTO member_of VALUES ('dhani', 'dewa', 1986), ('andra', 'dewa', 1986)",
        "INSERT INTO performed VALUES ('dewa', 'kirana', '1998-05-01', 'Jakarta')",
        "INSERT INTO belongs_to VALUES ('kirana', 'dewa')",
    ] {
        run(&mut db, dml);
    }
    run(&mut db, "COMMIT");
    // Songs written by members of the band that owns them.
    let got = rows(
        &mut db,
        "SELECT * FROM GRAPH_TABLE (music
           MATCH (w IS artist)-[:wrote]->(s IS song)-[:belongs_to]->(b IS artist WHERE b.kind = 'band'),
                 (w)-[:member_of]->(b)
           RETURN DISTINCT s.title AS song, b.name AS band, w.name AS writer)",
    );
    assert_eq!(sorted_texts(got), ["Kirana|Dewa|Andra", "Kirana|Dewa|Dhani"]);
    // The same edges under `base`: the property graph is a definition over
    // the base graph, not a second copy.
    let got = rows(
        &mut db,
        "SELECT * FROM GRAPH_TABLE (base MATCH (a IS artist)-[m:member_of]->(b IS artist) RETURN a._key AS a, m.since AS since)",
    );
    assert_eq!(sorted_texts(got), ["andra|1986", "dhani|1986"]);
}

#[test]
fn insert_raises_what_postgresql_raises() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    music(&mut db);
    run(&mut db, "INSERT INTO wrote VALUES ('dhani', 'kirana')");
    assert_eq!(sqlstate(db.sql("INSERT INTO wrote VALUES ('dhani', 'kirana')", &[])), "23505");
    run(&mut db, "ROLLBACK");
    assert_eq!(
        sqlstate(db.sql("INSERT INTO wrote VALUES ('dhani', 'no-such-song')", &[])),
        "23503"
    );
    run(&mut db, "ROLLBACK");
    assert_eq!(sqlstate(db.sql("INSERT INTO wrote (artist_id) VALUES ('dhani')", &[])), "23502");
    run(&mut db, "ROLLBACK");
    // A second owner of one song: the key is the song alone.
    run(&mut db, "INSERT INTO belongs_to VALUES ('kirana', 'dewa')");
    run(&mut db, "COMMIT");
    assert_eq!(sqlstate(db.sql("INSERT INTO belongs_to VALUES ('kirana', 'dhani')", &[])), "23505");
    run(&mut db, "ROLLBACK");
    assert_eq!(rows(&mut db, "SELECT artist_id FROM belongs_to WHERE song_id = 'kirana'"), [[SqlValue::Text("dewa".into())]]);
}

#[test]
fn the_key_decides_how_many_edges_a_pair_may_have() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    music(&mut db);
    run(&mut db, "INSERT INTO performed VALUES ('dewa', 'kirana', '1998-05-01', 'Jakarta')");
    assert_eq!(
        sqlstate(db.sql("INSERT INTO performed VALUES ('dewa', 'kirana', '1998-05-01', 'Jakarta')", &[])),
        "23505"
    );
    run(&mut db, "ROLLBACK");
    run(&mut db, "INSERT INTO performed VALUES ('dewa', 'kirana', '1998-05-01', 'Jakarta')");
    run(&mut db, "INSERT INTO performed VALUES ('dewa', 'kirana', '1998-05-01', 'Bandung')");
    run(&mut db, "COMMIT");
    let got = rows(
        &mut db,
        "SELECT venue FROM performed WHERE artist_id = 'dewa' AND song_id = 'kirana' ORDER BY venue",
    );
    assert_eq!(got, [[SqlValue::Text("Bandung".into())], [SqlValue::Text("Jakarta".into())]]);
    let hops = rows(
        &mut db,
        "SELECT * FROM GRAPH_TABLE (music MATCH (a IS artist WHERE a._key = 'dewa')-[p:performed]->(s IS song) RETURN p.venue AS venue)",
    );
    assert_eq!(hops.len(), 2, "two edges between one pair, each walked");
}

#[test]
fn on_conflict_does_nothing_or_updates() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    music(&mut db);
    run(&mut db, "INSERT INTO member_of VALUES ('andra', 'dewa', 1986)");
    run(
        &mut db,
        "INSERT INTO member_of VALUES ('andra', 'dewa', 1990) ON CONFLICT (member_id, group_id) DO NOTHING",
    );
    assert_eq!(
        rows(&mut db, "SELECT since FROM member_of WHERE member_id = 'andra'"),
        [[SqlValue::Int(1986)]]
    );
    run(
        &mut db,
        "INSERT INTO member_of VALUES ('andra', 'dewa', 1988) ON CONFLICT (member_id, group_id) DO UPDATE SET since = EXCLUDED.since",
    );
    run(
        &mut db,
        "INSERT INTO member_of VALUES ('dhani', 'dewa', 1986) ON CONFLICT (member_id, group_id) DO UPDATE SET since = EXCLUDED.since",
    );
    run(&mut db, "COMMIT");
    let got = rows(&mut db, "SELECT member_id, since FROM member_of WHERE group_id = 'dewa' ORDER BY member_id");
    assert_eq!(sorted_texts(got), ["andra|1988", "dhani|1986"]);
    refused(
        db.sql(
            "INSERT INTO member_of VALUES ('andra', 'dewa', 1) ON CONFLICT (member_id, group_id) DO UPDATE SET group_id = EXCLUDED.group_id",
            &[],
        ),
        "identity",
    );
}

#[test]
fn update_and_delete_name_an_end() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    music(&mut db);
    run(&mut db, "INSERT INTO member_of VALUES ('dhani', 'dewa', 1986), ('andra', 'dewa', 1986)");
    match run(&mut db, "UPDATE member_of SET since = 1988 WHERE member_id = 'andra' AND group_id = 'dewa'") {
        SqlResult::Affected(n) => assert_eq!(n, 1),
        other => panic!("{other:?}"),
    }
    refused(db.sql("UPDATE member_of SET since = 1 WHERE since = 1986", &[]), "must name");
    refused(db.sql("UPDATE member_of SET member_id = 'x' WHERE group_id = 'dewa'", &[]), "identity");
    run(&mut db, "ROLLBACK");
    run(&mut db, "INSERT INTO member_of VALUES ('dhani', 'dewa', 1986), ('andra', 'dewa', 1986)");
    run(&mut db, "UPDATE member_of SET since = 1988 WHERE member_id = 'andra'");
    match run(&mut db, "DELETE FROM member_of WHERE member_id = 'dhani'") {
        SqlResult::Affected(n) => assert_eq!(n, 1),
        other => panic!("{other:?}"),
    }
    refused(db.sql("DELETE FROM member_of WHERE since = 1988", &[]), "must name");
    run(&mut db, "COMMIT");
    let got = rows(&mut db, "SELECT member_id, since FROM member_of WHERE group_id = 'dewa'");
    assert_eq!(sorted_texts(got), ["andra|1988"]);
}

#[test]
fn select_reads_one_ends_edges() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    music(&mut db);
    for (on, venue) in [("2019-11-09", "Surabaya"), ("1998-05-01", "Jakarta"), ("2001-02-03", "Bali")] {
        db.sql(
            "INSERT INTO performed VALUES ($1, $2, $3, $4)",
            &[
                Param::Text("dewa".into()),
                Param::Text("kirana".into()),
                Param::Text(on.into()),
                Param::Text(venue.into()),
            ],
        )
        .unwrap();
    }
    run(&mut db, "COMMIT");
    let got = rows(
        &mut db,
        "SELECT performed_on, venue FROM performed WHERE artist_id = 'dewa' ORDER BY performed_on DESC LIMIT 2",
    );
    assert_eq!(
        got,
        [
            [SqlValue::Text("2019-11-09".into()), SqlValue::Text("Surabaya".into())],
            [SqlValue::Text("2001-02-03".into()), SqlValue::Text("Bali".into())],
        ]
    );
    let star = rows(&mut db, "SELECT * FROM performed WHERE song_id = 'kirana' AND venue = 'Jakarta'");
    assert_eq!(
        star,
        [[
            SqlValue::Text("dewa".into()),
            SqlValue::Text("kirana".into()),
            SqlValue::Text("1998-05-01".into()),
            SqlValue::Text("Jakarta".into()),
        ]]
    );
    let by_param = db
        .sql("SELECT venue FROM performed WHERE artist_id = $1 AND venue = $2", &[Param::Text("dewa".into()), Param::Text("Bali".into())])
        .unwrap();
    match by_param {
        SqlResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
    refused(db.sql("SELECT * FROM performed WHERE venue = 'Jakarta'", &[]), "must name");
}

#[test]
fn what_is_not_mapped_is_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    music(&mut db);
    // Declared, not yet in a property graph: no end is the source.
    run(&mut db, "CREATE TABLE covered (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song)");
    refused(db.sql("INSERT INTO covered VALUES ('dewa', 'kirana')", &[]), "CREATE PROPERTY GRAPH");
    // A composite key on a table of rows: a row has one key.
    refused(
        db.sql("CREATE TABLE pairs (a TEXT, b TEXT, PRIMARY KEY (a, b))", &[]),
        "edge table",
    );
    // REFERENCES names a row by its key.
    refused(
        db.sql("CREATE TABLE fans (artist_id TEXT REFERENCES artist (name))", &[]),
        "_key",
    );
    // A key over properties alone.
    refused(
        db.sql("CREATE TABLE gigs (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song, venue TEXT, PRIMARY KEY (venue))", &[]),
        "must name an end",
    );
    // The ends in the graph must be the table's own REFERENCES.
    refused(
        db.sql(
            "CREATE PROPERTY GRAPH other EDGE TABLES (covered SOURCE KEY (song_id) REFERENCES artist (_key) DESTINATION KEY (artist_id) REFERENCES song (_key))",
            &[],
        ),
        "references",
    );
    refused(db.sql("CREATE INDEX ON performed (venue)", &[]), "edge table");
    refused(db.sql("DROP TABLE performed", &[]), "edge table");
    refused(db.sql("DROP TABLE song", &[]), "references it");
    refused(
        db.sql("CREATE PROPERTY GRAPH g2 EDGE TABLES (covered SOURCE KEY (artist_id) REFERENCES artist (_key) DESTINATION KEY (song_id) REFERENCES song (_key) PROPERTIES (x))", &[]),
        "PROPERTIES",
    );
}

#[test]
fn drop_property_graph_keeps_the_edges() {
    let dir = TempDir::new().unwrap();
    let mut db = db(&dir);
    music(&mut db);
    run(&mut db, "INSERT INTO wrote VALUES ('dhani', 'kirana')");
    run(&mut db, "COMMIT");
    run(&mut db, "DROP PROPERTY GRAPH music");
    refused(db.sql("DROP PROPERTY GRAPH music", &[]), "music");
    run(&mut db, "DROP PROPERTY GRAPH IF EXISTS music");
    // The edges and the edge table stay.
    assert_eq!(rows(&mut db, "SELECT song_id FROM wrote WHERE artist_id = 'dhani'").len(), 1);
    run(&mut db, "INSERT INTO wrote VALUES ('andra', 'kirana')");
    let got = rows(
        &mut db,
        "SELECT * FROM GRAPH_TABLE (base MATCH (a IS artist)-[:wrote]->(s IS song) RETURN a._key AS a)",
    );
    assert_eq!(sorted_texts(got), ["andra", "dhani"]);
}
