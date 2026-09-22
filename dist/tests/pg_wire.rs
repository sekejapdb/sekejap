//! `docs/dist/WIRE_CONTRACT.md`, one test per rule, a BYTE at a time.
//!
//! Nothing here opens a socket. Every frame this file sends is built in this
//! file from the protocol's own layout -- a type byte, a self-inclusive
//! `Int32` length, a body -- and every reply is parsed back the same way, so
//! what is under test is the bytes on the wire and not a client library's
//! opinion of them. The end-to-end half, with a real PostgreSQL client
//! against the real `sekejap-pg` binary, is `dist/tests/pg_server.rs`.
//!
//! The oracle in every test is a brute-force fact held in this process: the
//! rows this file inserted, the SQLSTATE the contract's table names, the
//! number of commits this file made.

use kernel::io::IoMode;
use kernel::store::{Config, SyncMode};
use sekejap_core::collections::Database;
use sekejap_dist::pg::{
    connection::{BackendKey, CancelToken, Connection},
    types::oid,
    NOTIFY_REFUSAL,
};
use sekejap_dist::service::ServiceDatabase;
use std::time::Duration;
use tempfile::TempDir;

// ── the fixture ──────────────────────────────────────────────────────────

/// Rows the shared corpus holds. Large enough that a full scan is real work
/// -- which is what gives the deadline and the cancel something to stop --
/// and small enough that building it is milliseconds.
const ROWS: usize = 4_000;

fn config() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

struct Fixture {
    _dir: TempDir,
    service: ServiceDatabase,
}

/// The corpus, as this file knows it: `k000000..` with `born` counting up.
fn corpus_born(at: usize) -> i64 {
    1_900 + (at as i64 % 120)
}

fn build(rows: usize) -> Fixture {
    let dir = TempDir::new().expect("a temp dir");
    let path = dir.path().join("db");
    {
        use sekejap_lang::SqlDatabase;
        let mut db = Database::create(&path, config()).expect("create");
        db.sql(
            "CREATE TABLE place (id TEXT PRIMARY KEY, name TEXT, n INT, born INT, alive BOOL)",
            &[],
        )
        .expect("create table");
        // QL_CONTRACT §6: a Tier-1 predicate is answered INDEX-side, so a
        // filter or an order over a column with no index is refused rather
        // than turned into a scan. `n` is the unique ordinal this file
        // orders by; `born` is the value it filters on.
        db.sql("CREATE INDEX place_n ON place USING btree (n)", &[])
            .expect("index n");
        db.sql("CREATE INDEX place_born ON place USING btree (born)", &[])
            .expect("index born");
        for at in 0..rows {
            db.sql(
                &format!(
                    "INSERT INTO place (id, name, n, born, alive) VALUES ('k{at:06}', 'place {at:06}', {at}, {}, {})",
                    corpus_born(at),
                    if at % 2 == 0 { "true" } else { "false" }
                ),
                &[],
            )
            .expect("insert");
        }
        db.commit().expect("commit");
    }
    let service = ServiceDatabase::open(&path, config()).expect("open service");
    // A wire session reads its own writes on the next statement, which is
    // §2's window set to zero and one snapshot mint per commit.
    service.set_publish_interval(Duration::ZERO);
    Fixture { _dir: dir, service }
}

fn key(pid: i32) -> BackendKey {
    BackendKey {
        pid,
        secret: 0x5eca_0000 ^ pid,
    }
}

// ── frames this file builds ──────────────────────────────────────────────

fn startup() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for (k, v) in [("user", "sekejap"), ("database", "sekejap")] {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut frame = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    frame
}

fn ssl_request() -> Vec<u8> {
    let mut frame = 8i32.to_be_bytes().to_vec();
    frame.extend_from_slice(&80_877_103i32.to_be_bytes());
    frame
}

fn cancel_request(target: BackendKey) -> Vec<u8> {
    let mut frame = 16i32.to_be_bytes().to_vec();
    frame.extend_from_slice(&80_877_102i32.to_be_bytes());
    frame.extend_from_slice(&target.pid.to_be_bytes());
    frame.extend_from_slice(&target.secret.to_be_bytes());
    frame
}

fn framed(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![typ];
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn cstring(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

fn query(sql: &str) -> Vec<u8> {
    let mut body = Vec::new();
    cstring(&mut body, sql);
    framed(b'Q', &body)
}

fn parse_message(name: &str, sql: &str, oids: &[i32]) -> Vec<u8> {
    let mut body = Vec::new();
    cstring(&mut body, name);
    cstring(&mut body, sql);
    body.extend_from_slice(&(oids.len() as i16).to_be_bytes());
    for oid in oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    framed(b'P', &body)
}

fn bind_message(
    portal: &str,
    statement: &str,
    params: &[Option<&[u8]>],
    result_formats: &[i16],
) -> Vec<u8> {
    let mut body = Vec::new();
    cstring(&mut body, portal);
    cstring(&mut body, statement);
    body.extend_from_slice(&0i16.to_be_bytes()); // every parameter in text
    body.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for param in params {
        match param {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    body.extend_from_slice(&(result_formats.len() as i16).to_be_bytes());
    for format in result_formats {
        body.extend_from_slice(&format.to_be_bytes());
    }
    framed(b'B', &body)
}

fn describe_message(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    cstring(&mut body, name);
    framed(b'D', &body)
}

fn execute_message(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = Vec::new();
    cstring(&mut body, portal);
    body.extend_from_slice(&max_rows.to_be_bytes());
    framed(b'E', &body)
}

fn sync_message() -> Vec<u8> {
    framed(b'S', &[])
}

// ── frames this file reads back ──────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    typ: u8,
    body: Vec<u8>,
}

/// Split a reply into whole frames. A leading single `N` -- the `SSLRequest`
/// refusal, which carries no length -- is returned as a frame with an empty
/// body, because that is what it is.
fn frames(bytes: &[u8]) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] == b'N' && bytes.len() - at == 1 {
            out.push(Frame {
                typ: b'N',
                body: Vec::new(),
            });
            break;
        }
        if at + 5 > bytes.len() {
            break;
        }
        let len = i32::from_be_bytes([
            bytes[at + 1],
            bytes[at + 2],
            bytes[at + 3],
            bytes[at + 4],
        ]) as usize;
        let end = at + 1 + len;
        if end > bytes.len() {
            break;
        }
        out.push(Frame {
            typ: bytes[at],
            body: bytes[at + 5..end].to_vec(),
        });
        at = end;
    }
    out
}

fn types_of(frames: &[Frame]) -> Vec<char> {
    frames.iter().map(|f| f.typ as char).collect()
}

fn first(frames: &[Frame], typ: u8) -> Option<&Frame> {
    frames.iter().find(|f| f.typ == typ)
}

/// `(SQLSTATE, message)` of an `ErrorResponse` or a `NoticeResponse`.
fn error_fields(frame: &Frame) -> (String, String) {
    let mut sqlstate = String::new();
    let mut message = String::new();
    let mut at = 0usize;
    while at < frame.body.len() && frame.body[at] != 0 {
        let code = frame.body[at];
        at += 1;
        let start = at;
        while at < frame.body.len() && frame.body[at] != 0 {
            at += 1;
        }
        let value = String::from_utf8_lossy(&frame.body[start..at]).into_owned();
        at += 1;
        match code {
            b'C' => sqlstate = value,
            b'M' => message = value,
            _ => {}
        }
    }
    (sqlstate, message)
}

/// The `(name, type OID)` pairs of a `RowDescription`.
fn columns(frame: &Frame) -> Vec<(String, i32)> {
    let mut out = Vec::new();
    let count = i16::from_be_bytes([frame.body[0], frame.body[1]]) as usize;
    let mut at = 2usize;
    for _ in 0..count {
        let start = at;
        while frame.body[at] != 0 {
            at += 1;
        }
        let name = String::from_utf8_lossy(&frame.body[start..at]).into_owned();
        at += 1;
        let type_oid = i32::from_be_bytes([
            frame.body[at + 6],
            frame.body[at + 7],
            frame.body[at + 8],
            frame.body[at + 9],
        ]);
        at += 18; // table oid 4, column 2, type 4, size 2, modifier 4, format 2
        out.push((name, type_oid));
    }
    out
}

/// The cells of a `DataRow`, as text. `None` is SQL NULL.
fn cells(frame: &Frame) -> Vec<Option<String>> {
    let count = i16::from_be_bytes([frame.body[0], frame.body[1]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut at = 2usize;
    for _ in 0..count {
        let len = i32::from_be_bytes([
            frame.body[at],
            frame.body[at + 1],
            frame.body[at + 2],
            frame.body[at + 3],
        ]);
        at += 4;
        if len < 0 {
            out.push(None);
            continue;
        }
        let end = at + len as usize;
        out.push(Some(
            String::from_utf8_lossy(&frame.body[at..end]).into_owned(),
        ));
        at = end;
    }
    out
}

fn rows_of(frames: &[Frame]) -> Vec<Vec<Option<String>>> {
    frames
        .iter()
        .filter(|f| f.typ == b'D')
        .map(cells)
        .collect()
}

fn tag(frames: &[Frame]) -> String {
    first(frames, b'C')
        .map(|f| {
            let end = f.body.iter().position(|b| *b == 0).unwrap_or(f.body.len());
            String::from_utf8_lossy(&f.body[..end]).into_owned()
        })
        .unwrap_or_default()
}

/// Start a connection and swallow the banner.
fn connect<'a>(service: &'a ServiceDatabase, pid: i32) -> Connection<'a> {
    let mut connection = Connection::new(service, key(pid), CancelToken::new());
    let banner = connection.feed(&startup());
    assert_eq!(
        types_of(&frames(&banner)).last(),
        Some(&'Z'),
        "the banner ends with ReadyForQuery"
    );
    connection
}

/// Everything one simple `Query` produced, minus the trailing
/// `ReadyForQuery`.
fn ask(connection: &mut Connection<'_>, sql: &str) -> Vec<Frame> {
    let reply = connection.feed(&query(sql));
    let mut out = frames(&reply);
    assert_eq!(
        out.last().map(|f| f.typ),
        Some(b'Z'),
        "every simple Query ends with ReadyForQuery"
    );
    out.pop();
    // A refusal is an answer this file asserts about, so when one is being
    // investigated `PG_WIRE_DUMP=1` prints the SQLSTATE and the message
    // rather than leaving a reader to work them back out of a byte count.
    if let Some(error) = first(&out, b'E') {
        if std::env::var("PG_WIRE_DUMP").is_ok() {
            eprintln!("[dump] `{sql}` -> {:?}", error_fields(error));
        }
    }
    out
}

// ── startup ──────────────────────────────────────────────────────────────

/// A client reads `server_version`, `client_encoding`, `DateStyle`,
/// `integer_datetimes` and `TimeZone` off the banner before it will send a
/// statement, and it reads `BackendKeyData` because a cancel is out of band.
#[test]
fn the_startup_exchange_hands_out_the_parameters_and_the_cancel_key_a_client_needs() {
    let fixture = build(4);
    let mut connection = Connection::new(&fixture.service, key(7), CancelToken::new());
    let reply = connection.feed(&startup());
    let got = frames(&reply);

    assert_eq!(got.first().map(|f| f.typ), Some(b'R'), "AuthenticationOk");
    assert_eq!(
        got.first().map(|f| f.body.clone()),
        Some(0i32.to_be_bytes().to_vec()),
        "trust auth is AuthenticationOk with code 0"
    );

    let mut parameters = std::collections::HashMap::new();
    for frame in got.iter().filter(|f| f.typ == b'S') {
        let split = frame.body.iter().position(|b| *b == 0).expect("a key");
        let name = String::from_utf8_lossy(&frame.body[..split]).into_owned();
        let rest = &frame.body[split + 1..];
        let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        parameters.insert(name, String::from_utf8_lossy(&rest[..end]).into_owned());
    }
    assert_eq!(parameters.get("server_version").map(String::as_str), Some("16.0"));
    assert_eq!(parameters.get("client_encoding").map(String::as_str), Some("UTF8"));
    assert_eq!(parameters.get("DateStyle").map(String::as_str), Some("ISO, MDY"));
    assert_eq!(parameters.get("integer_datetimes").map(String::as_str), Some("on"));
    assert_eq!(parameters.get("TimeZone").map(String::as_str), Some("UTC"));

    let key_data = first(&got, b'K').expect("BackendKeyData");
    let pid = i32::from_be_bytes([
        key_data.body[0],
        key_data.body[1],
        key_data.body[2],
        key_data.body[3],
    ]);
    let secret = i32::from_be_bytes([
        key_data.body[4],
        key_data.body[5],
        key_data.body[6],
        key_data.body[7],
    ]);
    assert_eq!(
        BackendKey { pid, secret },
        key(7),
        "the pair on the wire is the pair this backend answers a CancelRequest for"
    );

    let ready = got.last().expect("ReadyForQuery");
    assert_eq!((ready.typ, ready.body.as_slice()), (b'Z', b"I".as_slice()));
}

/// §9.4: there is no TLS. The single byte `N` is the whole answer, and the
/// session then continues in plaintext with an ordinary startup.
#[test]
fn an_ssl_request_is_declined_with_one_byte_and_the_session_continues_in_plaintext() {
    let fixture = build(4);
    let mut connection = Connection::new(&fixture.service, key(1), CancelToken::new());

    let declined = connection.feed(&ssl_request());
    assert_eq!(declined, b"N", "one byte, no frame, no length");
    assert!(!connection.is_closed(), "the session is still open");

    let banner = connection.feed(&startup());
    assert_eq!(
        types_of(&frames(&banner)).last(),
        Some(&'Z'),
        "the plaintext startup that follows is answered normally"
    );
}

// ── the simple protocol ──────────────────────────────────────────────────

/// One simple `Query` is `RowDescription`, then one `DataRow` per row, then
/// `CommandComplete` carrying `SELECT <n>`. The rows are the rows this file
/// wrote, and the `born` column is typed `int8` because the CATALOG says
/// the column is `INT` -- not because the values happen to be whole numbers.
#[test]
fn a_simple_query_describes_its_columns_from_the_catalog_and_then_sends_the_rows() {
    let fixture = build(ROWS);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(
        &mut connection,
        "SELECT id, n, alive FROM place WHERE n < 5 ORDER BY n LIMIT 5",
    );
    assert_eq!(
        types_of(&got),
        vec!['T', 'D', 'D', 'D', 'D', 'D', 'C'],
        "description, five rows, tag"
    );
    assert_eq!(
        columns(first(&got, b'T').expect("RowDescription")),
        vec![
            ("id".to_owned(), oid::TEXT),
            ("n".to_owned(), oid::INT8),
            ("alive".to_owned(), oid::BOOL),
        ]
    );
    let rows = rows_of(&got);
    // The oracle: this file wrote `n = at`, so `n < 5` is the first five
    // keys in `n` order.
    let expected: Vec<String> = (0..5).map(|at| format!("k{at:06}")).collect();
    assert_eq!(
        rows.iter()
            .map(|row| row[0].clone().expect("a key"))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(rows[0][1], Some("0".to_owned()));
    assert_eq!(
        rows[0][2],
        Some("t".to_owned()),
        "a boolean rides the wire as PostgreSQL prints it: t or f"
    );
    assert_eq!(tag(&got), "SELECT 5");
}

/// A simple `Query` may carry several statements, and PostgreSQL abandons
/// the rest of the string at the first error. Both halves are tested with
/// one string.
#[test]
fn a_multi_statement_query_runs_in_order_and_stops_at_the_first_error() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(
        &mut connection,
        "INSERT INTO place (id, name, n, born, alive) VALUES ('m1', 'one', 9001, 2001, true); \
         INSERT INTO place (id, name, n, born, alive) VALUES ('m2', 'two', 9002, 2002, false)",
    );
    assert_eq!(types_of(&got), vec!['C', 'C'], "two tags, no error");
    assert_eq!(tag(&got), "INSERT 0 1");

    let got = ask(
        &mut connection,
        "INSERT INTO place (id, name, n, born, alive) VALUES ('m3', 'three', 9003, 2003, true); \
         SELECT id FROM nowhere; \
         INSERT INTO place (id, name, n, born, alive) VALUES ('m4', 'four', 9004, 2004, true)",
    );
    assert_eq!(
        types_of(&got),
        vec!['C', 'E'],
        "the first statement ran, the second failed, the third never ran"
    );

    let got = ask(&mut connection, "SELECT id FROM place WHERE born = 2004");
    assert!(
        rows_of(&got).is_empty(),
        "the statement after the error wrote nothing"
    );
}

// ── the extended protocol ────────────────────────────────────────────────

/// Parse, Bind with a typed `$n`, Describe, Execute, Sync -- and the row the
/// parameter names comes back. `Describe('S')` answers the parameter OIDs
/// the Parse declared, which is what a driver reads before it encodes a
/// value.
#[test]
fn an_extended_query_binds_a_declared_parameter_and_returns_the_row_it_names() {
    let fixture = build(ROWS);
    let mut connection = connect(&fixture.service, 1);

    let mut batch = parse_message(
        "s1",
        "SELECT id, born FROM place WHERE _key = $1",
        &[oid::TEXT],
    );
    batch.extend_from_slice(&describe_message(b'S', "s1"));
    batch.extend_from_slice(&bind_message("p1", "s1", &[Some(b"k000007")], &[0]));
    batch.extend_from_slice(&execute_message("p1", 0));
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));

    assert_eq!(
        types_of(&got),
        vec!['1', 't', 'T', '2', 'D', 'C', 'Z'],
        "ParseComplete, ParameterDescription, RowDescription, BindComplete, one row, tag, ready"
    );
    let description = first(&got, b't').expect("ParameterDescription");
    assert_eq!(
        i16::from_be_bytes([description.body[0], description.body[1]]),
        1,
        "one parameter"
    );
    assert_eq!(
        i32::from_be_bytes([
            description.body[2],
            description.body[3],
            description.body[4],
            description.body[5],
        ]),
        oid::TEXT,
        "the OID the Parse declared is the OID the Describe answers"
    );
    let rows = rows_of(&got);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some("k000007".to_owned()));
    assert_eq!(rows[0][1], Some(corpus_born(7).to_string()));
}

/// The same compiled statement, re-bound. A prepared statement outlives its
/// portals and the second Bind parses nothing.
#[test]
fn a_prepared_statement_is_rebound_with_new_parameters_without_being_parsed_again() {
    let fixture = build(ROWS);
    let mut connection = connect(&fixture.service, 1);

    let mut batch = parse_message("s1", "SELECT born FROM place WHERE _key = $1", &[oid::TEXT]);
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), vec!['1', 'Z']);

    for at in [3usize, 11, 130] {
        let mut batch = bind_message(
            "",
            "s1",
            &[Some(format!("k{at:06}").as_bytes())],
            &[0],
        );
        batch.extend_from_slice(&execute_message("", 0));
        batch.extend_from_slice(&sync_message());
        let got = frames(&connection.feed(&batch));
        assert_eq!(
            types_of(&got),
            vec!['2', 'D', 'C', 'Z'],
            "no ParseComplete: the statement was compiled once"
        );
        assert_eq!(
            rows_of(&got)[0][0],
            Some(corpus_born(at).to_string()),
            "each bind answers for ITS parameter"
        );
    }
}

/// `Execute` with a row limit hands out that many rows and answers
/// `PortalSuspended`; the next `Execute` on the same portal resumes where it
/// stopped, and the last one completes with the tag.
#[test]
fn a_portal_with_a_row_limit_suspends_and_resumes_until_the_rows_run_out() {
    let fixture = build(ROWS);
    let mut connection = connect(&fixture.service, 1);

    let mut batch = parse_message("s1", "SELECT id FROM place WHERE n < 5 ORDER BY n", &[]);
    batch.extend_from_slice(&bind_message("p1", "s1", &[], &[0]));
    batch.extend_from_slice(&execute_message("p1", 2));
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(
        types_of(&got),
        vec!['1', '2', 'D', 'D', 's', 'Z'],
        "two rows then PortalSuspended"
    );
    assert_eq!(
        rows_of(&got)
            .iter()
            .map(|row| row[0].clone().expect("a key"))
            .collect::<Vec<_>>(),
        vec!["k000000".to_owned(), "k000001".to_owned()]
    );

    let mut batch = execute_message("p1", 2);
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(types_of(&got), vec!['D', 'D', 's', 'Z']);
    assert_eq!(
        rows_of(&got)
            .iter()
            .map(|row| row[0].clone().expect("a key"))
            .collect::<Vec<_>>(),
        vec!["k000002".to_owned(), "k000003".to_owned()],
        "the second Execute resumes where the first stopped"
    );

    let mut batch = execute_message("p1", 2);
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(
        types_of(&got),
        vec!['D', 'C', 'Z'],
        "the fifth row is the last, so this Execute completes rather than suspends"
    );
    assert_eq!(tag(&got), "SELECT 1", "the tag counts THIS Execute's rows");
}

/// A count field arrives as an `Int16`. `0xFFFF` is `-1`, and believing it
/// as a `usize` is a `Vec::with_capacity(usize::MAX)`. Every count is
/// checked against what the frame can hold before it is believed.
#[test]
fn a_bind_that_claims_more_parameters_than_the_frame_holds_is_refused_not_believed() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    let mut batch = parse_message("s1", "SELECT id FROM place WHERE _key = $1", &[oid::TEXT]);
    // Hand-built Bind: portal, statement, 0 formats, then a parameter count
    // of 0xFFFF with no parameters behind it.
    let mut body = Vec::new();
    cstring(&mut body, "p1");
    cstring(&mut body, "s1");
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&(-1i16).to_be_bytes());
    batch.extend_from_slice(&framed(b'B', &body));
    batch.extend_from_slice(&sync_message());

    let got = frames(&connection.feed(&batch));
    let error = first(&got, b'E').expect("an ErrorResponse rather than an allocation");
    let (sqlstate, message) = error_fields(error);
    assert_eq!(sqlstate, "08P01", "protocol_violation");
    assert!(
        message.contains("parameter count"),
        "the refusal names what was malformed: {message}"
    );
    assert!(!connection.is_closed(), "the session survives a bad frame");
}

// ── the SQLSTATE map ─────────────────────────────────────────────────────

/// A Tier-2 or Tier-3 construct is `0A000 feature_not_supported` carrying
/// the contract's own reason. Never emulated, never a different code.
#[test]
fn a_refused_construct_arrives_as_0a000_with_the_contracts_named_reason() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(
        &mut connection,
        "SELECT id FROM place UNION SELECT id FROM place",
    );
    let (sqlstate, message) = error_fields(first(&got, b'E').expect("an ErrorResponse"));
    assert_eq!(sqlstate, "0A000");
    assert!(
        message.contains("UNION") && message.contains("Tier 3"),
        "the refusal names the construct and its tier: {message}"
    );
    assert!(
        message.contains("no atomic"),
        "and carries the contract's reason: {message}"
    );
}

/// A statement over a collection the catalog does not have is
/// `42P01 undefined_table`.
#[test]
fn a_statement_over_a_collection_that_is_not_there_arrives_as_42p01() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(&mut connection, "SELECT id FROM nowhere");
    let (sqlstate, message) = error_fields(first(&got, b'E').expect("an ErrorResponse"));
    assert_eq!(sqlstate, "42P01");
    assert!(message.contains("nowhere"), "{message}");
}

/// Text that does not spell a statement is `42601 syntax_error`, and it
/// arrives at `Parse` rather than three messages later at `Execute`.
#[test]
fn text_that_does_not_spell_a_statement_arrives_as_42601_at_parse_time() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(&mut connection, "SELEKT id FROM place");
    let (sqlstate, _) = error_fields(first(&got, b'E').expect("an ErrorResponse"));
    assert_eq!(sqlstate, "42601");

    let mut batch = parse_message("bad", "SELEKT id FROM place", &[]);
    batch.extend_from_slice(&sync_message());
    let got = frames(&connection.feed(&batch));
    assert_eq!(
        types_of(&got),
        vec!['E', 'Z'],
        "no ParseComplete: the refusal belongs to Parse"
    );
    assert_eq!(
        error_fields(first(&got, b'E').expect("an ErrorResponse")).0,
        "42601"
    );
}

/// §9.2 and §4: a cancel is `57014 query_canceled`, which is what `psql`'s
/// Ctrl-C and `Statement.cancel()` already expect.
#[test]
fn a_cancelled_statement_arrives_as_57014() {
    let fixture = build(ROWS);
    let token = CancelToken::new();
    let mut connection = Connection::new(&fixture.service, key(1), token.clone());
    let _ = connection.feed(&startup());

    // The token is sticky, so a statement issued after it is fired stops at
    // its first check point rather than racing a second thread.
    token.cancel();
    let got = ask(&mut connection, "SELECT id, name FROM place");
    let (sqlstate, message) = error_fields(first(&got, b'E').expect("an ErrorResponse"));
    assert_eq!(sqlstate, "57014");
    assert!(
        message.contains("canceling statement"),
        "the message is the one a client prints: {message}"
    );

    // A cancel stops the statement IN FLIGHT and nothing after it, which is
    // PostgreSQL's rule: the backend is idle at a `ReadyForQuery`, so the
    // connection's own token is cleared there and the next statement answers
    // with no second call. (§4's SERVICE handle is sticky by contract and is
    // a different thing; this is the per-backend one §9.2 routes to.)
    assert!(
        !token.is_cancelled(),
        "the ReadyForQuery that ended the refused statement cleared it"
    );
    let got = ask(&mut connection, "SELECT id FROM place WHERE n < 3 ORDER BY n");
    assert_eq!(rows_of(&got).len(), 3, "the connection is usable again");
}

/// §9.1 and §3: a statement that outruns `statement_timeout` is also
/// `57014`, and the two are told apart by the MESSAGE, which carries the
/// microseconds the refusing page had spent.
#[test]
fn a_statement_that_outruns_the_timeout_arrives_as_57014_with_the_elapsed_microseconds() {
    let fixture = build(ROWS);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(&mut connection, "SET statement_timeout = '1ms'");
    assert_eq!(tag(&got), "SET");
    assert_eq!(
        fixture.service.statement_timeout(),
        Some(Duration::from_millis(1)),
        "§9.1: the GUC reached ServiceDatabase::set_statement_timeout"
    );

    let got = ask(&mut connection, "SELECT id, name, born FROM place");
    let (sqlstate, message) = error_fields(first(&got, b'E').expect("an ErrorResponse"));
    assert_eq!(sqlstate, "57014");
    assert!(
        message.contains("statement timeout") && message.contains("microseconds"),
        "the timeout message carries the elapsed micros, which is what tells it \
         apart from a cancel: {message}"
    );

    // `SHOW` prints it back, and `0` clears it.
    let got = ask(&mut connection, "SHOW statement_timeout");
    assert_eq!(rows_of(&got)[0][0], Some("1ms".to_owned()));
    let _ = ask(&mut connection, "SET statement_timeout = 0");
    assert_eq!(fixture.service.statement_timeout(), None);
    let got = ask(&mut connection, "SELECT id FROM place LIMIT 2");
    assert_eq!(rows_of(&got).len(), 2, "cleared, the scan completes");
}

/// A `CancelRequest` arrives on its own connection, carries the pair
/// `BackendKeyData` handed out, and closes. The sans-IO engine reports the
/// pair; routing it is the server's, which is what makes the engine
/// testable without a socket.
#[test]
fn a_cancel_request_names_the_backend_it_wants_stopped_and_then_closes() {
    let fixture = build(4);
    let target = key(42);

    let mut carrier = Connection::new(&fixture.service, key(43), CancelToken::new());
    let reply = carrier.feed(&cancel_request(target));
    assert!(reply.is_empty(), "a CancelRequest is never answered");
    assert!(carrier.is_closed(), "and the connection closes");
    assert_eq!(
        carrier.cancel_request(),
        Some(target),
        "the pair the server must route to that backend's token"
    );
}

// ── transactions ─────────────────────────────────────────────────────────

/// `BEGIN` takes the service's single writer and `ReadyForQuery` reports
/// `T`; a failed statement inside the block reports `E` and refuses the
/// rest; `ROLLBACK` leaves nothing written.
#[test]
fn a_transaction_block_reports_its_status_and_a_rollback_leaves_no_row() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    let status = |reply: &[u8]| -> u8 {
        let got = frames(reply);
        got.last().expect("ReadyForQuery").body[0]
    };

    assert_eq!(status(&connection.feed(&query("BEGIN"))), b'T');
    let _ = connection.feed(&query(
        "INSERT INTO place (id, name, n, born, alive) VALUES ('t1', 'in block', 9101, 3001, true)",
    ));
    // Inside the block the writer is held, so the row is visible only to
    // this session's writer and not to a published snapshot.
    assert_eq!(
        status(&connection.feed(&query("SELECT id FROM place WHERE n < 2 ORDER BY n"))),
        b'T'
    );

    let reply = connection.feed(&query("SELECT id FROM nowhere"));
    assert_eq!(status(&reply), b'E', "a failed statement aborts the block");
    let reply = connection.feed(&query(
        "INSERT INTO place (id, name, n, born, alive) VALUES ('t2', 'after', 9102, 3002, true)",
    ));
    let got = frames(&reply);
    assert_eq!(
        error_fields(first(&got, b'E').expect("an ErrorResponse")).0,
        "25P02",
        "commands are ignored until the end of an aborted block"
    );

    assert_eq!(status(&connection.feed(&query("ROLLBACK"))), b'I');
    let got = ask(&mut connection, "SELECT id FROM place WHERE born = 3001");
    assert!(rows_of(&got).is_empty(), "a rollback wrote nothing");

    // And a committed block does write.
    let _ = connection.feed(&query("BEGIN"));
    let _ = connection.feed(&query(
        "INSERT INTO place (id, name, n, born, alive) VALUES ('t3', 'kept', 9103, 3003, true)",
    ));
    assert_eq!(status(&connection.feed(&query("COMMIT"))), b'I');
    let got = ask(&mut connection, "SELECT id FROM place WHERE born = 3003");
    assert_eq!(rows_of(&got).len(), 1, "a commit wrote one row");
}

// ── §9.3 LISTEN / NOTIFY ─────────────────────────────────────────────────

/// §9.3: a notification is emitted by a COMMITTED batch, never before it,
/// and a rolled-back batch emits nothing. One per listening channel.
#[test]
fn listen_delivers_one_notification_per_listening_channel_after_each_commit_and_none_before() {
    let fixture = build(4);
    let mut listener = connect(&fixture.service, 1);
    let mut writer = connect(&fixture.service, 2);

    let got = ask(&mut listener, "LISTEN rows");
    assert_eq!(tag(&got), "LISTEN");
    let got = ask(&mut listener, "LISTEN also");
    assert_eq!(tag(&got), "LISTEN");

    // Nothing has committed since the LISTEN.
    assert!(
        listener.poll_notify().is_empty(),
        "no commit, no notification"
    );

    // An uncommitted batch emits nothing.
    let _ = writer.feed(&query("BEGIN"));
    let _ = writer.feed(&query(
        "INSERT INTO place (id, name, n, born, alive) VALUES ('n1', 'pending', 9201, 4001, true)",
    ));
    assert!(
        listener.poll_notify().is_empty(),
        "§9.3: delivery is at the END of the transaction, not when the write runs"
    );
    let _ = writer.feed(&query("ROLLBACK"));
    assert!(
        listener.poll_notify().is_empty(),
        "§9.3: a rolled-back transaction delivers nothing"
    );

    // A committed one emits exactly one notification per channel.
    let _ = writer.feed(&query(
        "INSERT INTO place (id, name, n, born, alive) VALUES ('n2', 'committed', 9202, 4002, true)",
    ));
    let push = listener.poll_notify();
    let got = frames(&push);
    assert_eq!(
        types_of(&got),
        vec!['A', 'A'],
        "one NotificationResponse per listening channel, for one committed batch"
    );

    let mut channels = Vec::new();
    for frame in &got {
        let rest = &frame.body[4..];
        let end = rest.iter().position(|b| *b == 0).expect("a channel");
        channels.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        let payload = &rest[end + 1..];
        let payload_end = payload.iter().position(|b| *b == 0).unwrap_or(payload.len());
        let payload = String::from_utf8_lossy(&payload[..payload_end]).into_owned();
        assert!(
            payload.contains("sequence=") && payload.contains("keys="),
            "the payload names the batch and the key count (§9.3): {payload}"
        );
        assert!(
            payload.len() <= 8_000,
            "the protocol caps a payload at 8000 bytes"
        );
    }
    channels.sort();
    assert_eq!(channels, vec!["also".to_owned(), "rows".to_owned()]);

    // UNLISTEN ends it.
    let got = ask(&mut listener, "UNLISTEN *");
    assert_eq!(tag(&got), "UNLISTEN");
    let _ = writer.feed(&query(
        "INSERT INTO place (id, name, n, born, alive) VALUES ('n3', 'after', 9203, 4003, true)",
    ));
    assert!(
        listener.poll_notify().is_empty(),
        "an unlistened session hears nothing"
    );
}

/// A client-issued `NOTIFY` is REFUSED by name: §9.3 makes the CHANGE FEED
/// the source of notifications, and there is no second queue to write into.
/// Refused, with the reason, never emulated.
#[test]
fn a_client_issued_notify_is_refused_by_name_rather_than_emulated() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(&mut connection, "NOTIFY rows, 'hello'");
    let (sqlstate, message) = error_fields(first(&got, b'E').expect("an ErrorResponse"));
    assert_eq!(sqlstate, "0A000");
    assert_eq!(message, NOTIFY_REFUSAL);
}

// ── cursors (QL_CONTRACT §2) ─────────────────────────────────────────────

/// `DECLARE` / `FETCH FORWARD n` / `CLOSE` inside one session, over the
/// pages of a prepared query. `FETCH` past the end returns nothing and does
/// not fail, as PostgreSQL's does.
#[test]
fn a_declared_cursor_fetches_forward_in_pages_and_then_closes() {
    let fixture = build(ROWS);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(
        &mut connection,
        "DECLARE c CURSOR FOR SELECT id FROM place WHERE n < 5 ORDER BY n",
    );
    assert_eq!(tag(&got), "DECLARE CURSOR");

    let got = ask(&mut connection, "FETCH FORWARD 2 FROM c");
    assert_eq!(
        rows_of(&got)
            .iter()
            .map(|row| row[0].clone().expect("a key"))
            .collect::<Vec<_>>(),
        vec!["k000000".to_owned(), "k000001".to_owned()]
    );
    assert_eq!(tag(&got), "FETCH 2");

    let got = ask(&mut connection, "FETCH 2 c");
    assert_eq!(
        rows_of(&got)
            .iter()
            .map(|row| row[0].clone().expect("a key"))
            .collect::<Vec<_>>(),
        vec!["k000002".to_owned(), "k000003".to_owned()],
        "a FETCH resumes where the last one stopped"
    );

    let got = ask(&mut connection, "FETCH ALL FROM c");
    assert_eq!(rows_of(&got).len(), 1, "one row was left");
    let got = ask(&mut connection, "FETCH ALL FROM c");
    assert!(rows_of(&got).is_empty(), "past the end is empty, not an error");

    let got = ask(&mut connection, "CLOSE c");
    assert_eq!(tag(&got), "CLOSE CURSOR");
    let got = ask(&mut connection, "FETCH 1 FROM c");
    assert_eq!(
        error_fields(first(&got, b'E').expect("an ErrorResponse")).0,
        "34000",
        "a closed cursor is invalid_cursor_name"
    );
}

// ── what the wire refuses ────────────────────────────────────────────────

/// The catalog views are `docs/dist/PG_SURFACE.md`'s and `sekejap_lang`
/// answers them as virtual rows, so a client's schema-tree query flows
/// through the wire and comes back as rows, not as a refusal.
#[test]
fn a_catalog_query_flows_through_to_the_catalog_views_and_answers_rows() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    for sql in [
        "SELECT relname FROM pg_catalog.pg_class",
        "SELECT table_name FROM information_schema.tables",
    ] {
        let got = ask(&mut connection, sql);
        assert!(first(&got, b'E').is_none(), "`{sql}` was refused");
        assert!(!rows_of(&got).is_empty(), "`{sql}` answered no row");
    }
}

/// The fixed session rows a client sends before it will talk: `version()`
/// and the `current_*` functions. Constants, not queries, and named as such.
#[test]
fn the_fixed_session_rows_a_client_reads_at_connect_are_answered() {
    let fixture = build(4);
    let mut connection = connect(&fixture.service, 1);

    let got = ask(&mut connection, "SELECT version()");
    let version = rows_of(&got)[0][0].clone().expect("a version");
    assert!(version.starts_with("PostgreSQL 16.0 (sekejap "), "{version}");

    let got = ask(&mut connection, "SELECT current_schema()");
    assert_eq!(rows_of(&got)[0][0], Some("public".to_owned()));

    let got = ask(&mut connection, "SET client_encoding = 'UTF8'");
    assert_eq!(tag(&got), "SET");
    let got = ask(&mut connection, "SHOW client_encoding");
    assert_eq!(rows_of(&got)[0][0], Some("UTF8".to_owned()));
}

/// `Terminate` closes the session, and a transaction still open at that
/// point ROLLS BACK: a close is not a commit (`OPS_CONTRACT` §1).
#[test]
fn terminate_closes_the_session_and_rolls_back_a_transaction_still_open() {
    let fixture = build(4);
    {
        let mut connection = connect(&fixture.service, 1);
        let _ = connection.feed(&query("BEGIN"));
        let _ = connection.feed(&query(
            "INSERT INTO place (id, name, n, born, alive) VALUES ('x1', 'dropped', 9301, 5001, true)",
        ));
        let reply = connection.feed(&framed(b'X', &[]));
        assert!(reply.is_empty(), "Terminate is not answered");
        assert!(connection.is_closed());
    }
    let mut after = connect(&fixture.service, 2);
    let got = ask(&mut after, "SELECT id FROM place WHERE born = 5001");
    assert!(
        rows_of(&got).is_empty(),
        "a close is not a commit: the open batch rolled back"
    );
}
