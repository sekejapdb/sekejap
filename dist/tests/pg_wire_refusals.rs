//! A refusal that has a NAME never reaches a client as `XX000
//! internal_error`.
//!
//! `XX000` is the one SQLSTATE a PostgreSQL client retries: it means "the
//! server hit something it did not understand about itself", and a driver's
//! pool will hand the same statement back. Four refusals of
//! `docs/lang/QL_CONTRACT.md` -- a `SHOW` word that names neither a
//! collection nor a client setting, a predicate on a column with no scalar
//! index, a fold with no expression index, and a boolean leaf with no
//! membership set -- were each raised with the contract's own sentence and
//! then given that code, because `dist/src/pg/types.rs::sql_error` did not
//! recognise the message and fell through to its default (§7 item 9).
//! Retrying any of them can only produce the same refusal again.
//!
//! The right code for all four is `0A000 feature_not_supported`, which is
//! what the contract's own `sql refused` blocks claim. This file asserts it
//! through the SAME path the server uses -- `pg::Connection::feed`, a real
//! `Query` frame in and a real `ErrorResponse` frame out -- rather than
//! through a second opinion about what that path would do.
//!
//! The oracle is the contract: the SQLSTATE each block of §8 names, held here
//! as a literal beside the statement it belongs to.

use kernel::io::IoMode;
use kernel::store::{Config, SyncMode};
use sekejap_core::collections::Database;
use sekejap_dist::pg::connection::{BackendKey, CancelToken, Connection};
use sekejap_dist::service::ServiceDatabase;
use std::time::Duration;
use tempfile::TempDir;

// ── the fixture ──────────────────────────────────────────────────────────

/// `place` in the shape `docs/lang/EXAMPLE_FIXTURE.md` gives it, cut to the
/// columns these refusals need: `kind` indexed so a boolean leaf beside it is
/// admissible, `area` a geometry with a gist index so the geometry leaf is
/// reached rather than refused for a missing index, `name` for the fold and
/// `note` with NO index at all, so a predicate on it has something to name.
const DDL: &[&str] = &[
    "CREATE TABLE place (id TEXT PRIMARY KEY, kind TEXT, name TEXT, note TEXT, \
     loc GEOMETRY(Point,4326), area GEOMETRY(Polygon,4326))",
    "CREATE INDEX place_kind ON place USING btree (kind)",
    "CREATE INDEX place_loc ON place USING gist (loc)",
    "CREATE INDEX place_area ON place USING gist (area)",
];

struct Fixture {
    _dir: TempDir,
    service: ServiceDatabase,
}

fn config() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn build() -> Fixture {
    let dir = TempDir::new().expect("a temp dir");
    let path = dir.path().join("db");
    {
        use sekejap_lang::SqlDatabase;
        let mut db = Database::create(&path, config()).expect("create");
        for statement in DDL {
            db.sql(statement, &[]).unwrap_or_else(|e| panic!("`{statement}`: {e}"));
        }
        db.commit().expect("commit");
    }
    let service = ServiceDatabase::open(&path, config()).expect("open service");
    service.set_publish_interval(Duration::ZERO);
    Fixture { _dir: dir, service }
}

// ── the protocol, built and read here ────────────────────────────────────

fn framed(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![typ];
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn startup() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for (key, value) in [("user", "wire"), ("database", "db")] {
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut out = Vec::new();
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn query(sql: &str) -> Vec<u8> {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    framed(b'Q', &body)
}

/// `(type, SQLSTATE, message)` of every frame in one reply that carries an
/// error or a notice. Whole frames only: a truncated tail is a bug in this
/// file's own framing and would be read as "no error", so it is refused.
fn errors(bytes: &[u8]) -> Vec<(u8, String, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 5 <= bytes.len() {
        let len = i32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
            as usize;
        let end = at + 1 + len;
        assert!(end <= bytes.len(), "a frame runs past the reply");
        let typ = bytes[at];
        if typ == b'E' || typ == b'N' {
            let body = &bytes[at + 5..end];
            let (mut sqlstate, mut message) = (String::new(), String::new());
            let mut i = 0usize;
            while i < body.len() && body[i] != 0 {
                let code = body[i];
                i += 1;
                let start = i;
                while i < body.len() && body[i] != 0 {
                    i += 1;
                }
                let value = String::from_utf8_lossy(&body[start..i]).into_owned();
                i += 1;
                match code {
                    b'C' => sqlstate = value,
                    b'M' => message = value,
                    _ => {}
                }
            }
            out.push((typ, sqlstate, message));
        }
        at = end;
    }
    out
}

fn connect(service: &ServiceDatabase) -> Connection<'_> {
    let mut connection = Connection::new(
        service,
        BackendKey {
            pid: 1,
            secret: 0x5E_5E_5E_5Eu32 as i32,
        },
        CancelToken::new(),
    );
    let banner = connection.feed(&startup());
    assert!(!banner.is_empty(), "the startup exchange answers");
    connection
}

/// The `(SQLSTATE, message)` one statement produces over the wire.
fn wire(connection: &mut Connection<'_>, sql: &str) -> (String, String) {
    let reply = connection.feed(&query(sql));
    let found = errors(&reply);
    let error = found
        .iter()
        .find(|(typ, _, _)| *typ == b'E')
        .unwrap_or_else(|| panic!("`{sql}` produced no ErrorResponse: {found:?}"));
    (error.1.clone(), error.2.clone())
}

// ── the four ─────────────────────────────────────────────────────────────

/// `feature_not_supported`. The code every `sql refused` block of
/// `QL_CONTRACT` §8 claims, and the code these four now carry.
const FEATURE_NOT_SUPPORTED: &str = "0A000";
/// `internal_error`. Written out here because it is the thing under test: it
/// is what a client RETRIES, and none of these four may carry it.
const INTERNAL_ERROR: &str = "XX000";

/// The four refusals `QL_CONTRACT` §7 item 9 names, each over the wire, each
/// with the SQLSTATE the contract's own block claims and each still carrying
/// the sentence that names WHY.
#[test]
fn a_named_refusal_the_engine_raises_never_arrives_as_an_internal_error() {
    let fixture = build();
    let mut connection = connect(&fixture.service);

    for (sql, must_say) in [
        // §2: a SHOW word that is neither a collection nor a client setting.
        ("SHOW STATUS", "no collection of that name"),
        // §3: a predicate on a column with no scalar index is not demoted to
        // a scan.
        (
            "SELECT id FROM place WHERE note = 'x'",
            "a scalar index on `note` does not exist",
        ),
        // §4.1: a fold with no expression index is refused, not scanned.
        (
            "SELECT id FROM place WHERE lower(name) = 'x'",
            "an expression index",
        ),
        // §3: a boolean leaf with no membership set. The one of the four
        // whose cause is in the ENGINE rather than in `lang`.
        (
            "SELECT id FROM place WHERE kind = 'port' OR ST_Contains(area, ST_SetSRID(ST_MakePoint(106.82, -6.17), 4326))",
            "cannot be a boolean leaf",
        ),
    ] {
        let (sqlstate, message) = wire(&mut connection, sql);
        assert_ne!(
            sqlstate, INTERNAL_ERROR,
            "`{sql}` is a NAMED refusal and arrived as the one code a client retries: {message}"
        );
        assert_eq!(
            sqlstate, FEATURE_NOT_SUPPORTED,
            "`{sql}` should carry the code QL_CONTRACT §8 claims: {message}"
        );
        assert!(
            message.contains(must_say),
            "`{sql}` should still say why: {message}"
        );
        println!("{sqlstate}  {sql}");
    }
}

/// The codes that are NOT `0A000` are unmoved. A fix that made every refusal
/// `feature_not_supported` would pass the test above and destroy the wire
/// contract's table, so the neighbours are asserted beside it.
#[test]
fn the_neighbouring_sqlstates_are_unmoved_by_the_four_new_arms() {
    let fixture = build();
    let mut connection = connect(&fixture.service);

    for (sql, code) in [
        // A collection the catalog does not have is still undefined_table.
        ("SELECT id FROM nowhere", "42P01"),
        // Malformed text is still a syntax error, over the wire as well as
        // in `lang`.
        ("SELCT id FROM place", "42601"),
        // A Tier-3 construct is feature_not_supported, as it always was.
        (
            "SELECT id FROM place WHERE kind = 'a' INTERSECT SELECT id FROM place WHERE kind = 'b'",
            "0A000",
        ),
        // A collection that already exists is still duplicate_table.
        (
            "CREATE TABLE place (id TEXT PRIMARY KEY, kind TEXT)",
            "42P07",
        ),
    ] {
        let (sqlstate, message) = wire(&mut connection, sql);
        assert_eq!(sqlstate, code, "`{sql}`: {message}");
        println!("{sqlstate}  {sql}");
    }
}

/// The refusal a client reads is the one `sekejap_lang` raised: the same
/// sentence, not a summary of it. A code without the reason would tell a
/// caller to stop without telling them what to write instead.
#[test]
fn the_wire_carries_the_whole_reason_and_not_only_the_code() {
    let fixture = build();
    let mut connection = connect(&fixture.service);

    let (sqlstate, message) = wire(&mut connection, "SELECT id FROM place WHERE note = 'x'");
    assert_eq!(sqlstate, FEATURE_NOT_SUPPORTED);
    for fragment in [
        "QL_CONTRACT §6",
        "answered index-side",
        "NAMES an index",
    ] {
        assert!(
            message.contains(fragment),
            "the wire dropped `{fragment}`: {message}"
        );
    }

    let (sqlstate, message) = wire(&mut connection, "SHOW STATUS");
    assert_eq!(sqlstate, FEATURE_NOT_SUPPORTED);
    assert!(
        message.contains("SHOW TABLES"),
        "the refusal still says what to write instead: {message}"
    );
}
