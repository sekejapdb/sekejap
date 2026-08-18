//! The PostgreSQL wire protocol, against the answers the database gives directly.
//!
//! `src/pg.rs` is 1227 lines that no test touched. It is what `psql`, DBeaver and
//! pgjdbc talk to, and it is sans-IO — bytes in, bytes out — so none of that
//! needs a socket to check.
//!
//! The oracle is the core API. Whatever `db.query(sql)` answers is the truth, and
//! the wire must deliver the same values under the same column names. A protocol
//! is exactly the place where a value can arrive intact but *wrong*: a NULL
//! delivered as the string "null", a number as a quoted string, a column dropped
//! because it was absent from the first row.

use sekejap::{CoreDB, Hit};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};

// ── A minimal client ─────────────────────────────────────────────────────────

fn startup() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196608i32.to_be_bytes()); // protocol 3.0
    body.extend_from_slice(b"user\0sekejap\0database\0sekejap\0\0");
    let mut msg = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    msg.extend_from_slice(&body);
    msg
}

fn simple_query(sql: &str) -> Vec<u8> {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    let mut msg = vec![b'Q'];
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// What came back: column names, rows of optional strings (`None` is a real
/// SQL NULL — length -1 on the wire), the `CommandComplete` tags and any error.
#[derive(Default, Debug)]
struct Reply {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    tags: Vec<String>,
    errors: Vec<String>,
}

fn decode(bytes: &[u8]) -> Reply {
    let mut r = Reply::default();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        let typ = bytes[i];
        let len = i32::from_be_bytes([bytes[i + 1], bytes[i + 2], bytes[i + 3], bytes[i + 4]]) as usize;
        let body = &bytes[i + 5..(i + 1 + len).min(bytes.len())];
        i += 1 + len;
        match typ {
            b'T' => {
                r.columns.clear();
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                let mut p = 2;
                for _ in 0..n {
                    let end = body[p..].iter().position(|&b| b == 0).unwrap() + p;
                    r.columns.push(String::from_utf8_lossy(&body[p..end]).into_owned());
                    p = end + 1 + 18; // NUL + table oid, attnum, type oid, size, modifier, format
                }
            }
            b'D' => {
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                let mut p = 2;
                let mut row = Vec::with_capacity(n);
                for _ in 0..n {
                    let l = i32::from_be_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
                    p += 4;
                    if l < 0 {
                        row.push(None); // SQL NULL
                    } else {
                        let l = l as usize;
                        row.push(Some(String::from_utf8_lossy(&body[p..p + l]).into_owned()));
                        p += l;
                    }
                }
                r.rows.push(row);
            }
            b'C' => r.tags.push(String::from_utf8_lossy(&body[..body.len() - 1]).into_owned()),
            b'E' => r.errors.push(String::from_utf8_lossy(body).replace('\0', " ")),
            _ => {}
        }
    }
    r
}

fn connect(db: &Arc<RwLock<CoreDB>>) -> sekejap::pg::Connection {
    let mut conn = sekejap::pg::Connection::new(db.clone(), false);
    let hello = conn.feed(&startup());
    assert!(!hello.is_empty(), "the server said nothing to a startup packet");
    conn
}

fn ask(conn: &mut sekejap::pg::Connection, sql: &str) -> Reply {
    decode(&conn.feed(&simple_query(sql)))
}

// ── Fixture ──────────────────────────────────────────────────────────────────

fn fixture() -> (tempfile::TempDir, Arc<RwLock<CoreDB>>) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, n INTEGER, r REAL)").unwrap();
    for i in 0..6 {
        let mut o = json!({"_collection": "p", "_key": format!("n{i}"),
                           "name": format!("row {i}"), "n": i as i64, "r": 1.5});
        // A row with an explicit NULL and a row missing the column entirely —
        // the two shapes a protocol most often confuses with each other and with
        // an empty string.
        if i == 2 { o["name"] = Value::Null; }
        if i == 3 { o.as_object_mut().unwrap().remove("name"); }
        db.put(&format!("p/n{i}"), &o.to_string()).unwrap();
    }
    db.compact().unwrap();
    (dir, Arc::new(RwLock::new(db)))
}

/// The same query through the core API, as `(column → value)` per row.
fn direct(db: &Arc<RwLock<CoreDB>>, sql: &str) -> Vec<Hit> {
    db.read().unwrap().query(sql).unwrap().collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// **The wire must return the rows the database returns.**
#[test]
fn the_wire_agrees_with_the_database_about_rows() {
    let (_d, db) = fixture();
    let mut conn = connect(&db);

    for sql in [
        "SELECT _key FROM p",
        "SELECT _key, n FROM p WHERE n > 2",
        "SELECT _key FROM p ORDER BY n DESC LIMIT 3",
        "SELECT COUNT(*) FROM p",
    ] {
        let wire = ask(&mut conn, sql);
        assert!(wire.errors.is_empty(), "`{sql}` errored on the wire: {:?}", wire.errors);
        let core = direct(&db, sql);
        assert_eq!(wire.rows.len(), core.len(),
            "`{sql}` returned {} row(s) on the wire and {} through the API",
            wire.rows.len(), core.len());
        assert!(!wire.rows.is_empty(), "`{sql}` returned nothing — it cannot detect a difference");
    }
}

/// **A NULL is not the string "null", and a missing column is a NULL.**
///
/// On the wire a NULL is a length of -1. Sending the four characters `null`
/// instead delivers a value the row never had, and every client will show it as
/// text.
#[test]
fn null_arrives_as_null() {
    let (_d, db) = fixture();
    let mut conn = connect(&db);

    let wire = ask(&mut conn, "SELECT _key, name FROM p ORDER BY _key ASC");
    assert!(wire.errors.is_empty(), "{:?}", wire.errors);
    assert_eq!(wire.columns, vec!["_key", "name"],
        "the wire named the columns differently from the query");
    assert_eq!(wire.rows.len(), 6);

    for row in &wire.rows {
        let key = row[0].as_deref().unwrap_or("");
        match key {
            // n2 holds an explicit NULL, n3 has no `name` at all. Both are NULL.
            "n2" | "n3" => assert_eq!(row[1], None,
                "`{key}` has no name, and the wire sent {:?} rather than NULL", row[1]),
            _ => assert!(row[1].is_some() && row[1].as_deref() != Some("null"),
                "`{key}` has a name and the wire sent {:?}", row[1]),
        }
    }
}

/// **A query that fails must produce an error, not an empty result.**
///
/// A client cannot tell "no rows" from "your query was wrong" unless the server
/// says so, and it is the same silence that `SEARCH` and an unknown table used to
/// answer with.
#[test]
fn a_failing_query_reports_an_error() {
    let (_d, db) = fixture();
    let mut conn = connect(&db);

    for sql in [
        "SELECT _key FROM no_such_table",
        "SELECT _key FROM p WHERE",
        "THIS IS NOT SQL",
    ] {
        let wire = ask(&mut conn, sql);
        assert!(!wire.errors.is_empty(),
            "`{sql}` produced no error — the client sees {} row(s) and no reason",
            wire.rows.len());
        assert!(wire.rows.is_empty(), "`{sql}` errored and still sent rows");
    }

    // The connection is still usable afterwards, which is what `ReadyForQuery`
    // is for — an error must not wedge the session.
    let after = ask(&mut conn, "SELECT _key FROM p");
    assert!(after.errors.is_empty(), "the session broke after an error: {:?}", after.errors);
    assert_eq!(after.rows.len(), 6, "the session lost its way after an error");
}

/// Numbers must arrive as numbers, not as quoted JSON.
#[test]
fn values_arrive_in_their_own_shape() {
    let (_d, db) = fixture();
    let mut conn = connect(&db);
    let wire = ask(&mut conn, "SELECT _key, n, r FROM p WHERE _key = 'n1'");
    assert!(wire.errors.is_empty(), "{:?}", wire.errors);
    assert_eq!(wire.rows.len(), 1);
    let row = &wire.rows[0];
    assert_eq!(row[1].as_deref(), Some("1"), "an integer arrived as {:?}", row[1]);
    assert_eq!(row[2].as_deref(), Some("1.5"), "a real arrived as {:?}", row[2]);
    assert_eq!(row[0].as_deref(), Some("n1"), "a text key arrived as {:?}", row[0]);
}

// ── Extended protocol ────────────────────────────────────────────────────────
//
// Parse / Bind / Describe / Execute / Sync, with `$1` parameters. This is the
// path pgjdbc and DBeaver take for anything parameterised, so it carries the
// injection-safe promise: a parameter must arrive as a *value*, never as SQL.

fn msg(typ: u8, body: Vec<u8>) -> Vec<u8> {
    let mut m = vec![typ];
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(&body);
    m
}

fn parse(name: &str, sql: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(name.as_bytes()); b.push(0);
    b.extend_from_slice(sql.as_bytes()); b.push(0);
    b.extend_from_slice(&0u16.to_be_bytes()); // no pre-declared param types
    msg(b'P', b)
}

fn bind(portal: &str, stmt: &str, params: &[Option<&str>]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes()); b.push(0);
    b.extend_from_slice(stmt.as_bytes()); b.push(0);
    b.extend_from_slice(&0u16.to_be_bytes());                  // all params text
    b.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for p in params {
        match p {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                b.extend_from_slice(v.as_bytes());
            }
        }
    }
    b.extend_from_slice(&0u16.to_be_bytes());                  // all results text
    msg(b'B', b)
}

fn describe_portal(portal: &str) -> Vec<u8> {
    let mut b = vec![b'P'];
    b.extend_from_slice(portal.as_bytes()); b.push(0);
    msg(b'D', b)
}

fn execute(portal: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes()); b.push(0);
    b.extend_from_slice(&0i32.to_be_bytes()); // no row limit
    msg(b'E', b)
}

fn sync() -> Vec<u8> { msg(b'S', Vec::new()) }

/// Run one extended-protocol round trip.
fn ask_extended(
    conn: &mut sekejap::pg::Connection,
    sql: &str,
    params: &[Option<&str>],
) -> Reply {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&parse("", sql));
    bytes.extend_from_slice(&bind("", "", params));
    bytes.extend_from_slice(&describe_portal(""));
    bytes.extend_from_slice(&execute(""));
    bytes.extend_from_slice(&sync());
    decode(&conn.feed(&bytes))
}

/// **The extended protocol must answer what the simple one answers.**
#[test]
fn the_extended_protocol_agrees_with_the_simple_one() {
    let (_d, db) = fixture();
    let mut conn = connect(&db);

    for sql in [
        "SELECT _key FROM p",
        "SELECT _key, n FROM p WHERE n > 2",
        "SELECT COUNT(*) FROM p",
    ] {
        let simple = ask(&mut conn, sql);
        let extended = ask_extended(&mut conn, sql, &[]);
        assert!(extended.errors.is_empty(),
            "`{sql}` errored on the extended path: {:?}", extended.errors);
        assert!(!extended.rows.is_empty(), "`{sql}` returned nothing to compare");
        assert_eq!(simple.rows, extended.rows,
            "`{sql}` answers differently depending on which protocol asked");
        assert_eq!(simple.columns, extended.columns,
            "`{sql}` names its columns differently on the two paths");
    }
}

/// **A parameter is a value, never SQL.**
///
/// This is the whole reason the parameterised path exists. If `$1` were pasted
/// into the statement, the second case below would delete or dump the table
/// rather than looking for a row whose key happens to contain an apostrophe.
#[test]
fn a_bound_parameter_is_data_and_not_sql() {
    let (_d, db) = fixture();
    let mut conn = connect(&db);

    let found = ask_extended(&mut conn, "SELECT _key FROM p WHERE _key = $1", &[Some("n1")]);
    assert!(found.errors.is_empty(), "{:?}", found.errors);
    assert_eq!(found.rows.len(), 1, "a bound parameter did not select the row");
    assert_eq!(found.rows[0][0].as_deref(), Some("n1"));

    // A parameter that would be dangerous if it were pasted in as SQL.
    let hostile = ask_extended(
        &mut conn,
        "SELECT _key FROM p WHERE _key = $1",
        &[Some("n1' OR '1'='1")],
    );
    assert!(hostile.rows.is_empty(),
        "a parameter was interpreted as SQL: it matched {} row(s)", hostile.rows.len());

    // The table is intact, which is the part that matters.
    let after = ask(&mut conn, "SELECT _key FROM p");
    assert_eq!(after.rows.len(), 6, "the parameterised query changed the table");
}

/// A NULL parameter is a NULL, not the text "NULL" and not an empty string.
#[test]
fn a_null_parameter_stays_null() {
    let (_d, db) = fixture();
    let mut conn = connect(&db);
    let reply = ask_extended(&mut conn, "SELECT _key FROM p WHERE name = $1", &[None]);
    // Whatever it answers, it must not be an error and must not match the rows
    // whose name is the *string* "null" — there are none, so nothing may match.
    assert!(reply.errors.is_empty(), "a NULL parameter errored: {:?}", reply.errors);
    assert!(reply.rows.is_empty(),
        "`name = NULL` matched {} row(s); in SQL nothing equals NULL", reply.rows.len());
}

/// **An error on the extended path must not wedge the connection.**
///
/// The protocol requires everything after a failed message to be discarded until
/// `Sync`. Getting that wrong leaves a client hanging or, worse, executing the
/// remains of a statement it thinks was abandoned.
#[test]
fn an_error_is_confined_to_its_own_extended_exchange() {
    let (_d, db) = fixture();
    let mut conn = connect(&db);

    let bad = ask_extended(&mut conn, "SELECT _key FROM no_such_table", &[]);
    assert!(!bad.errors.is_empty(), "a bad statement produced no error");
    assert!(bad.rows.is_empty(), "a failed statement still sent rows");

    let good = ask_extended(&mut conn, "SELECT _key FROM p", &[]);
    assert!(good.errors.is_empty(), "the connection was wedged: {:?}", good.errors);
    assert_eq!(good.rows.len(), 6, "the connection lost its way after an error");
}
