//! The bytes: every PostgreSQL v3 message this surface reads or writes, and
//! nothing else.
//!
//! This file knows the FRAME and not the meaning. A message is a one-byte
//! type, an `Int32` length that counts itself, and a body; the startup frame
//! is the one exception and carries no type byte. Everything here either
//! appends one such frame to an output buffer or reads fields out of one
//! body, so the state machine in [`super::connection`] never touches an
//! index.
//!
//! **Why the reader returns zeros instead of refusing.** A body is a frame
//! the caller has already length-checked, so a read past its end means the
//! SENDER lied about what it packed, not that this buffer is short. The
//! reader saturates rather than panicking, and every COUNT read out of a
//! body is checked against [`Reader::remaining`] before it is believed --
//! which is what stops a two-byte `0xFFFF` from becoming a `usize::MAX`
//! allocation (e1 `src/pg.rs:249-286`, the same hole, fixed the same way).

/// The protocol version this surface speaks: 3.0.
pub const PROTOCOL_V3: i32 = 196_608;
/// `SSLRequest`, answered with a single `N` (no TLS -- `OPS_CONTRACT` §9.4).
pub const SSL_REQUEST: i32 = 80_877_103;
/// `GSSENCRequest`, answered with the same `N`.
pub const GSSAPI_REQUEST: i32 = 80_877_104;
/// `CancelRequest`, which arrives on a SECOND connection and carries the
/// `(process id, secret)` pair `BackendKeyData` handed out (§9.2).
pub const CANCEL_REQUEST: i32 = 80_877_102;

/// The largest startup frame accepted, in bytes. A startup packet is a short
/// list of `key\0value\0` pairs; a megabyte is four orders of magnitude more
/// than any client sends and is here so a hostile sender cannot name a
/// gigabyte and have it believed.
pub const MAX_STARTUP_LEN: usize = 1 << 20;
/// The largest ordinary frame accepted, in bytes. The protocol's own ceiling
/// is `i32::MAX`; this is 256 MiB, the same bound e1 set, for the same
/// reason.
pub const MAX_MSG_LEN: usize = 1 << 28;

// ── Backend message types (server -> client) ─────────────────────────────

pub const AUTHENTICATION: u8 = b'R';
pub const BACKEND_KEY_DATA: u8 = b'K';
pub const BIND_COMPLETE: u8 = b'2';
pub const CLOSE_COMPLETE: u8 = b'3';
pub const COMMAND_COMPLETE: u8 = b'C';
pub const DATA_ROW: u8 = b'D';
pub const EMPTY_QUERY_RESPONSE: u8 = b'I';
pub const ERROR_RESPONSE: u8 = b'E';
pub const NO_DATA: u8 = b'n';
pub const NOTICE_RESPONSE: u8 = b'N';
pub const NOTIFICATION_RESPONSE: u8 = b'A';
pub const PARAMETER_DESCRIPTION: u8 = b't';
pub const PARAMETER_STATUS: u8 = b'S';
pub const PARSE_COMPLETE: u8 = b'1';
pub const PORTAL_SUSPENDED: u8 = b's';
pub const READY_FOR_QUERY: u8 = b'Z';
pub const ROW_DESCRIPTION: u8 = b'T';

// ── Frontend message types (client -> server) ────────────────────────────

pub const F_BIND: u8 = b'B';
pub const F_CLOSE: u8 = b'C';
pub const F_DESCRIBE: u8 = b'D';
pub const F_EXECUTE: u8 = b'E';
pub const F_FLUSH: u8 = b'H';
pub const F_PARSE: u8 = b'P';
pub const F_QUERY: u8 = b'Q';
pub const F_SYNC: u8 = b'S';
pub const F_TERMINATE: u8 = b'X';

/// The transaction state `ReadyForQuery` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    /// `I` -- idle, no transaction block open.
    Idle,
    /// `T` -- inside a `BEGIN` block.
    InBlock,
    /// `E` -- inside a `BEGIN` block that has failed and will `ROLLBACK`.
    Failed,
}

impl TransactionStatus {
    pub fn byte(self) -> u8 {
        match self {
            Self::Idle => b'I',
            Self::InBlock => b'T',
            Self::Failed => b'E',
        }
    }
}

// ── Writers ──────────────────────────────────────────────────────────────

/// Append one framed message: type byte, self-inclusive `Int32` length, body.
pub fn msg(out: &mut Vec<u8>, typ: u8, body: &[u8]) {
    out.push(typ);
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
}

/// Append a NUL-terminated string.
pub fn cstr(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

pub fn authentication_ok(out: &mut Vec<u8>) {
    msg(out, AUTHENTICATION, &0i32.to_be_bytes());
}

pub fn parameter_status(out: &mut Vec<u8>, key: &str, value: &str) {
    let mut body = Vec::new();
    cstr(&mut body, key);
    cstr(&mut body, value);
    msg(out, PARAMETER_STATUS, &body);
}

/// The `(process id, secret)` pair a later `CancelRequest` quotes back.
pub fn backend_key_data(out: &mut Vec<u8>, pid: i32, secret: i32) {
    let mut body = Vec::new();
    body.extend_from_slice(&pid.to_be_bytes());
    body.extend_from_slice(&secret.to_be_bytes());
    msg(out, BACKEND_KEY_DATA, &body);
}

pub fn ready_for_query(out: &mut Vec<u8>, status: TransactionStatus) {
    msg(out, READY_FOR_QUERY, &[status.byte()]);
}

pub fn command_complete(out: &mut Vec<u8>, tag: &str) {
    let mut body = Vec::new();
    cstr(&mut body, tag);
    msg(out, COMMAND_COMPLETE, &body);
}

pub fn empty_query_response(out: &mut Vec<u8>) {
    msg(out, EMPTY_QUERY_RESPONSE, &[]);
}

pub fn no_data(out: &mut Vec<u8>) {
    msg(out, NO_DATA, &[]);
}

pub fn parse_complete(out: &mut Vec<u8>) {
    msg(out, PARSE_COMPLETE, &[]);
}

pub fn bind_complete(out: &mut Vec<u8>) {
    msg(out, BIND_COMPLETE, &[]);
}

pub fn close_complete(out: &mut Vec<u8>) {
    msg(out, CLOSE_COMPLETE, &[]);
}

pub fn portal_suspended(out: &mut Vec<u8>) {
    msg(out, PORTAL_SUSPENDED, &[]);
}

/// One column of a `RowDescription`.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldDescription {
    pub name: String,
    /// `pg_type.oid`. See `super::types`.
    pub type_oid: i32,
    /// `pg_type.typlen`: the fixed width, or `-1` for a varlena.
    pub type_size: i16,
    /// `0` for text, `1` for binary. The format this column will actually be
    /// sent in, which is what the protocol says this field means.
    pub format: i16,
}

pub fn row_description(out: &mut Vec<u8>, fields: &[FieldDescription]) {
    let mut body = Vec::new();
    body.extend_from_slice(&(fields.len() as i16).to_be_bytes());
    for field in fields {
        cstr(&mut body, &field.name);
        // Table OID and column attribute number: zero, meaning "not a column
        // of a table as the catalog knows it". A sekejap collection has no
        // `pg_class` row of its own; the catalog views that would give it one
        // are `docs/dist/PG_SURFACE.md`'s, not this file's.
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&field.type_oid.to_be_bytes());
        body.extend_from_slice(&field.type_size.to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        body.extend_from_slice(&field.format.to_be_bytes());
    }
    msg(out, ROW_DESCRIPTION, &body);
}

/// The `$n` type OIDs a `Describe('S')` answers with.
pub fn parameter_description(out: &mut Vec<u8>, oids: &[i32]) {
    let mut body = Vec::new();
    body.extend_from_slice(&(oids.len() as i16).to_be_bytes());
    for oid in oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    msg(out, PARAMETER_DESCRIPTION, &body);
}

/// One row. `None` is SQL NULL, which the wire spells as a length of `-1`
/// and NOT as a zero-length value.
pub fn data_row(out: &mut Vec<u8>, cells: &[Option<Vec<u8>>]) {
    let mut body = Vec::new();
    body.extend_from_slice(&(cells.len() as i16).to_be_bytes());
    for cell in cells {
        match cell {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    msg(out, DATA_ROW, &body);
}

/// `ErrorResponse`: severity, SQLSTATE, message, and nothing this surface
/// cannot say truthfully.
pub fn error_response(out: &mut Vec<u8>, sqlstate: &str, message: &str) {
    severity_response(out, ERROR_RESPONSE, "ERROR", sqlstate, message);
}

/// `NoticeResponse`, the same body under a different type byte. A `SET` of a
/// client GUC and every notice `sekejap_lang` attaches to a statement arrive
/// this way.
pub fn notice_response(out: &mut Vec<u8>, sqlstate: &str, message: &str) {
    severity_response(out, NOTICE_RESPONSE, "NOTICE", sqlstate, message);
}

fn severity_response(out: &mut Vec<u8>, typ: u8, severity: &str, sqlstate: &str, message: &str) {
    let mut body = Vec::new();
    body.push(b'S');
    cstr(&mut body, severity);
    body.push(b'V');
    cstr(&mut body, severity);
    body.push(b'C');
    cstr(&mut body, sqlstate);
    body.push(b'M');
    cstr(&mut body, message);
    body.push(0);
    msg(out, typ, &body);
}

/// `NotificationResponse`: the backend that committed, the channel, and the
/// payload. `OPS_CONTRACT` §9.3.
pub fn notification_response(out: &mut Vec<u8>, pid: i32, channel: &str, payload: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&pid.to_be_bytes());
    cstr(&mut body, channel);
    cstr(&mut body, payload);
    msg(out, NOTIFICATION_RESPONSE, &body);
}

// ── Reader ───────────────────────────────────────────────────────────────

/// A cursor over one message body.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Bytes still unread. A COUNT field says how many items follow; this
    /// says how many could possibly fit, and believing a count larger than
    /// this is how a two-byte field becomes an allocation.
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    pub fn byte(&mut self) -> u8 {
        let value = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        value
    }

    pub fn i16(&mut self) -> i16 {
        let mut buf = [0u8; 2];
        for slot in &mut buf {
            *slot = self.byte();
        }
        i16::from_be_bytes(buf)
    }

    pub fn i32(&mut self) -> i32 {
        let mut buf = [0u8; 4];
        for slot in &mut buf {
            *slot = self.byte();
        }
        i32::from_be_bytes(buf)
    }

    pub fn bytes(&mut self, n: usize) -> &'a [u8] {
        let end = self.at.saturating_add(n).min(self.bytes.len());
        let out = &self.bytes[self.at.min(self.bytes.len())..end];
        self.at = end;
        out
    }

    /// A NUL-terminated string. Invalid UTF-8 is replaced rather than
    /// refused: a name this surface cannot spell back is a name it will not
    /// find, which is already an error with a better message than "invalid
    /// encoding".
    pub fn cstr(&mut self) -> String {
        let start = self.at.min(self.bytes.len());
        while self.at < self.bytes.len() && self.bytes[self.at] != 0 {
            self.at += 1;
        }
        let text = String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned();
        if self.at < self.bytes.len() {
            self.at += 1;
        }
        text
    }
}

/// Read a NUL-terminated string from the front of a body -- a simple `Query`
/// is exactly one of these.
pub fn cstr_at_front(body: &[u8]) -> String {
    let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
    String::from_utf8_lossy(&body[..end]).into_owned()
}
