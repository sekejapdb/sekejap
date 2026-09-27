//! Edge tables over the PostgreSQL wire (`docs/core/EDGE_TABLES.md` §4): a
//! client gets PostgreSQL's own SQLSTATE for a refused edge write -- `23505`
//! for a taken key, `23503` for an end that names no row -- so a driver
//! handles it as it handles PostgreSQL's, and a read of one end's edges comes
//! back as rows.

use kernel::io::IoMode;
use kernel::store::{Config, SyncMode};
use sekejap_core::collections::Database;
use sekejap_dist::pg::connection::{BackendKey, CancelToken, Connection};
use sekejap_dist::service::ServiceDatabase;
use std::time::Duration;
use tempfile::TempDir;

const DDL: &[&str] = &[
    "CREATE TABLE artist (_key TEXT PRIMARY KEY, name TEXT)",
    "CREATE TABLE song (_key TEXT PRIMARY KEY, title TEXT)",
    "CREATE TABLE wrote (artist_id TEXT REFERENCES artist, song_id TEXT REFERENCES song, PRIMARY KEY (artist_id, song_id))",
    "INSERT INTO artist (_key, name) VALUES ('dhani', 'Dhani')",
    "INSERT INTO song (_key, title) VALUES ('kirana', 'Kirana')",
    "CREATE PROPERTY GRAPH music VERTEX TABLES (artist, song) EDGE TABLES (wrote SOURCE KEY (artist_id) REFERENCES artist (_key) DESTINATION KEY (song_id) REFERENCES song (_key))",
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

/// How many `DataRow` frames one reply carries.
fn data_rows(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut at = 0usize;
    while at + 5 <= bytes.len() {
        let len = i32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
            as usize;
        if bytes[at] == b'D' {
            count += 1;
        }
        at += 1 + len;
    }
    count
}

#[test]
fn an_edge_write_the_data_refuses_carries_postgresqls_sqlstate() {
    let fixture = build();
    let mut connection = connect(&fixture.service);
    let first = connection.feed(&query("INSERT INTO wrote VALUES ('dhani', 'kirana')"));
    assert!(
        errors(&first).iter().all(|(typ, _, _)| *typ != b'E'),
        "the first edge is written: {:?}",
        errors(&first)
    );
    let (sqlstate, message) = wire(&mut connection, "INSERT INTO wrote VALUES ('dhani', 'kirana')");
    assert_eq!(sqlstate, "23505", "{message}");
    let (sqlstate, message) = wire(&mut connection, "INSERT INTO wrote VALUES ('dhani', 'no-such-song')");
    assert_eq!(sqlstate, "23503", "{message}");
    assert!(message.contains("no-such-song"), "{message}");
}

#[test]
fn a_read_of_one_ends_edges_comes_back_as_rows() {
    let fixture = build();
    let mut connection = connect(&fixture.service);
    connection.feed(&query("INSERT INTO wrote VALUES ('dhani', 'kirana')"));
    let reply = connection.feed(&query("SELECT song_id FROM wrote WHERE artist_id = 'dhani'"));
    assert!(errors(&reply).iter().all(|(typ, _, _)| *typ != b'E'), "{:?}", errors(&reply));
    assert_eq!(data_rows(&reply), 1);
}
