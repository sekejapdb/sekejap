//! The PostgreSQL wire-protocol byte-framing helpers `dist/tests/pg_wire.rs`,
//! `dist/tests/pg_wire_gql.rs` and `dist/tests/pg_wire_gql_types.rs` share
//! (finding X3): building the frames a client sends and parsing the frames
//! a connection answers, from the protocol's own layout, one byte at a
//! time. Nothing here opens a socket -- callers drive an in-process
//! `sekejap_dist::pg::connection::Connection` directly.
#![allow(dead_code)]

use kernel::io::IoMode;
use kernel::store::{Config, SyncMode};
use sekejap_dist::pg::connection::{BackendKey, CancelToken, Connection};
use sekejap_dist::service::ServiceDatabase;

/// The `Config` every wire suite's fixture database opens with.
pub fn config() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// A `BackendKey` derived from `pid` alone, so two connections in the same
/// process never collide by accident.
pub fn key(pid: i32) -> BackendKey {
    BackendKey {
        pid,
        secret: 0x5eca_0000 ^ pid,
    }
}

// ── frames a client builds ───────────────────────────────────────────────

/// A protocol-3.0 `StartupMessage` for `user=sekejap database=sekejap`.
pub fn startup() -> Vec<u8> {
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

/// One `type byte, self-inclusive Int32 length, body` frame.
pub fn framed(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![typ];
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

pub fn cstring(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

pub fn query(sql: &str) -> Vec<u8> {
    let mut body = Vec::new();
    cstring(&mut body, sql);
    framed(b'Q', &body)
}

pub fn parse_message(name: &str, sql: &str, oids: &[i32]) -> Vec<u8> {
    let mut body = Vec::new();
    cstring(&mut body, name);
    cstring(&mut body, sql);
    body.extend_from_slice(&(oids.len() as i16).to_be_bytes());
    for oid in oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    framed(b'P', &body)
}

/// A `Bind`. `params` is one slot per parameter, `None` for SQL NULL,
/// `Some(bytes)` for the parameter's text or binary bytes (the caller
/// decides the format; every parameter here is sent in text format).
pub fn bind_message(
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

/// `kind` is `b'S'` for a statement, `b'P'` for a portal.
pub fn describe_message(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    cstring(&mut body, name);
    framed(b'D', &body)
}

pub fn execute_message(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = Vec::new();
    cstring(&mut body, portal);
    body.extend_from_slice(&max_rows.to_be_bytes());
    framed(b'E', &body)
}

pub fn sync_message() -> Vec<u8> {
    framed(b'S', &[])
}

// ── frames a client reads back ───────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub typ: u8,
    pub body: Vec<u8>,
}

/// Split a reply into whole frames. A leading single `N` -- the
/// `SSLRequest` refusal, which carries no length -- is returned as a frame
/// with an empty body, because that is what it is.
pub fn frames(bytes: &[u8]) -> Vec<Frame> {
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
        let len = i32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
            as usize;
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

pub fn types_of(frames: &[Frame]) -> Vec<char> {
    frames.iter().map(|f| f.typ as char).collect()
}

pub fn first(frames: &[Frame], typ: u8) -> Option<&Frame> {
    frames.iter().find(|f| f.typ == typ)
}

/// The `(name, type OID)` pairs of a `RowDescription`.
pub fn columns(frame: &Frame) -> Vec<(String, i32)> {
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

pub fn tag(frames: &[Frame]) -> String {
    first(frames, b'C')
        .map(|f| {
            let end = f.body.iter().position(|b| *b == 0).unwrap_or(f.body.len());
            String::from_utf8_lossy(&f.body[..end]).into_owned()
        })
        .unwrap_or_default()
}

/// Start a connection and swallow the banner.
pub fn connect(service: &ServiceDatabase, backend_key: BackendKey) -> Connection<'_> {
    let mut connection = Connection::new(service, backend_key, CancelToken::new());
    let banner = connection.feed(&startup());
    assert_eq!(
        types_of(&frames(&banner)).last(),
        Some(&'Z'),
        "the banner ends with ReadyForQuery"
    );
    connection
}
