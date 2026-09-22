//! One client connection's protocol state, SANS-IO: it is handed the bytes
//! that arrived and it hands back the bytes to send.
//!
//! No socket, no thread, no runtime. [`Connection::feed`] parses whole
//! frames out of an inbound buffer, runs whatever they ask for against the
//! service, and appends the reply; `dist/src/pg/server.rs` is the few dozen
//! lines that move those two buffers over TCP, and a caller in any other
//! language could move them over anything else.
//!
//! ## Where a statement goes
//!
//! Every statement that is not session chatter goes through
//! `sekejap_lang::prepare_sql_with` or `sekejap_lang::SqlDatabase::sql` over
//! [`ServiceDatabase`] (`docs/dist/OPS_CONTRACT.md` §1): a read takes this
//! connection's own snapshot handle, a write takes the service's single
//! writer. There is no second engine here, and the only statements this file
//! answers from data of its own are the fixed session rows a client sends
//! before it will talk at all.
//!
//! ## The four surfaces §9 names
//!
//! * **§9.1** `SET statement_timeout` routes to
//!   [`ServiceDatabase::set_statement_timeout`]. Stated consequence: §3's
//!   bound is per SERVICE, as §4's handle is ("per handle, and therefore per
//!   service"), so a session that sets it sets it for the server.
//! * **§9.2** `BackendKeyData` hands out a real `(pid, secret)` pair, and a
//!   `CancelRequest` quoting it is reported by
//!   [`Connection::cancel_request`] for the server to route to that
//!   backend's [`CancelToken`]. The token is ORed into the cancellation
//!   closure beside the service's own `InterruptHandle`, so a cancel can
//!   stop one connection without stopping the server.
//! * **§9.3** `LISTEN` subscribes this session to the change feed and each
//!   COMMITTED batch becomes one `NotificationResponse` per listening
//!   channel. `NOTIFY` as a client statement is REFUSED by name: §9.3 makes
//!   the feed the source of notifications, and there is no second queue for
//!   a client-issued `NOTIFY` to write into.
//! * **§9.4** No TLS. `SSLRequest` is answered `N` and the session continues
//!   in plaintext.
//!
//! ## What a suspended portal holds, and why
//!
//! `Execute` with a row limit, and `DECLARE`/`FETCH`, both need a walk to be
//! stopped and resumed BETWEEN protocol messages. e4's `PreparedQuery`
//! borrows the `Database` handle for the life of the walk and is handed out
//! through a callback that owns the request (`PreparedSql::with_query`), so
//! it cannot outlive one call, and there is no public resume cursor for a
//! read. A portal or cursor that is given a row limit therefore PAGES its
//! answer once -- through `PreparedSql::for_each_row_with`, so every page is
//! charged against the budget and sees the deadline and the cancel -- and
//! holds the rows it has not yet handed out. That buffer is bounded by
//! [`CURSOR_ROW_CAP`] and [`CURSOR_BYTES_CAP`], and an answer that passes
//! either is REFUSED naming the ceiling, never truncated. An `Execute` with
//! no row limit streams page by page and holds nothing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sekejap_core::collections::{CollectionId, Database, EntityId, QueryBudget};
use sekejap_lang::{Param, PreparedSql, SqlError, SqlResult, SqlRow, SqlValue};

use crate::service::{ChangeEvent, Receiver, ServiceDatabase, ServiceError, WriterGuard};

use super::frames::{self as f, FieldDescription, TransactionStatus};
use super::types::{self, oid, WireError};

/// Rows one suspended portal or open cursor may hold at once.
///
/// A stated ceiling, not a tuning knob (Law 1). 65,536 rows is four orders
/// of magnitude more than any interactive `FETCH` and small enough that the
/// buffer cannot become the process's memory profile; past it the statement
/// is refused naming this number.
pub const CURSOR_ROW_CAP: usize = 65_536;
/// Encoded bytes one suspended portal or open cursor may hold at once:
/// 16 MiB. The second half of the same bound, because 65,536 rows of one
/// column and 65,536 rows of a hundred are not the same quantity.
pub const CURSOR_BYTES_CAP: usize = 16 << 20;
/// Rows one page of a walk asks for. The same 8,192 `sekejap_lang` pages at.
const PAGE_ROWS: usize = 8_192;
/// The `server_version` this surface reports.
pub const SERVER_VERSION: &str = "16.0";

/// The reason a client-issued `NOTIFY` is refused.
pub const NOTIFY_REFUSAL: &str =
    "NOTIFY: OPS_CONTRACT §9.3 makes LISTEN/NOTIFY the CHANGE FEED's surface -- a notification \
     is emitted by a COMMITTED batch (§5), and there is no second notification queue for a \
     client-issued NOTIFY to write into. LISTEN on a channel and commit a write";
/// The `(process id, secret)` pair `BackendKeyData` hands out and a
/// `CancelRequest` quotes back (`OPS_CONTRACT` §9.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BackendKey {
    pub pid: i32,
    pub secret: i32,
}

/// One connection's own cancel flag, beside the service's `InterruptHandle`.
///
/// §4's handle is per service and stops every statement in flight; the
/// protocol's `CancelRequest` names ONE backend. This is what makes that
/// distinction expressible: firing it stops the statements of one connection
/// and no other, and the service's handle still stops them all.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Clear a standing cancel. Returns whether one was standing.
    pub fn clear(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

/// One statement's rows, as this statement's own columns.
#[derive(Clone, Debug)]
pub struct Answer {
    pub fields: Vec<FieldDescription>,
    pub rows: Vec<Vec<SqlValue>>,
    /// The `CommandComplete` tag, when it is not `SELECT <n>`: `FETCH <n>`.
    pub tag: Option<String>,
}

/// What running one statement produced.
#[derive(Clone, Debug)]
enum Outcome {
    /// Rows already held: an aggregate, a cursor `FETCH`, a fixed session
    /// row, or a portal that was given a row limit.
    Rows(Answer),
    /// A command tag and nothing else.
    Command(String),
    /// The statement was empty.
    Empty,
}

/// A prepared statement: the text, and the `$n` type OIDs the `Parse`
/// declared (empty when it declared none).
#[derive(Clone, Debug)]
struct Prepared {
    sql: String,
    param_oids: Vec<i32>,
}

/// A bound portal.
#[derive(Debug)]
struct Portal {
    sql: String,
    params: Vec<Param>,
    /// One code per result column, or one code for all, or empty for text.
    result_formats: Vec<i16>,
    /// Filled by the first `Describe` or limited `Execute` that needs rows.
    answer: Option<Answer>,
    /// A statement that returns no rows: the tag it completed with.
    tag: Option<String>,
    /// Rows already handed out of `answer`.
    delivered: usize,
    /// True once this portal has been run at all.
    executed: bool,
}

/// An open cursor (`QL_CONTRACT` §2, T2 -> T1 inside this session).
#[derive(Debug)]
struct Cursor {
    answer: Answer,
    delivered: usize,
}

/// One client connection's protocol state.
///
/// Borrows the service rather than holding an `Arc` of it, because a
/// transaction is an open [`WriterGuard`] and a guard borrows the service it
/// took the writer from. `dist/src/pg/server.rs` gives every connection
/// thread that borrow with `std::thread::scope`, which is also what makes
/// the server's shutdown a join rather than a leak.
pub struct Connection<'a> {
    service: &'a ServiceDatabase,
    backend: BackendKey,
    cancel: CancelToken,
    /// This connection's own read view, and the published serial it was
    /// minted at. `OPS_CONTRACT` §1: one snapshot reader per connection, so
    /// two connections walk at once instead of taking turns on one handle.
    reader: Option<crate::service::Snapshot>,
    reader_at: u64,
    started: bool,
    closed: bool,
    /// A `CancelRequest` this connection carried, for the server to route.
    cancel_request: Option<BackendKey>,
    inbuf: Vec<u8>,
    statements: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    cursors: HashMap<String, Cursor>,
    /// The open transaction, as the service's single writer, held.
    txn: Option<WriterGuard<'a>>,
    /// Set when a statement inside a `BEGIN` block failed: the block reports
    /// `E` and refuses everything but `COMMIT` / `ROLLBACK`, as PostgreSQL
    /// does.
    txn_failed: bool,
    /// After an error inside an extended-protocol batch, everything up to
    /// the next `Sync` is skipped.
    skip_until_sync: bool,
    /// Session GUCs, as `SHOW` prints them back.
    gucs: HashMap<String, String>,
    /// §9.3: the channels this session listens on, and its feed subscription.
    listening: Vec<String>,
    changes: Option<Receiver>,
    /// Notifications drained from the feed but not yet written out.
    pending_notifications: Vec<(String, String)>,
}

impl<'a> Connection<'a> {
    /// Start a connection over `service`, identified by `backend`.
    pub fn new(service: &'a ServiceDatabase, backend: BackendKey, cancel: CancelToken) -> Self {
        Self {
            service,
            backend,
            cancel,
            reader: None,
            reader_at: 0,
            started: false,
            closed: false,
            cancel_request: None,
            inbuf: Vec::new(),
            statements: HashMap::new(),
            portals: HashMap::new(),
            cursors: HashMap::new(),
            txn: None,
            txn_failed: false,
            skip_until_sync: false,
            gucs: default_gucs(),
            listening: Vec::new(),
            changes: None,
            pending_notifications: Vec::new(),
        }
    }

    /// The pair this connection published in `BackendKeyData`.
    pub fn backend_key(&self) -> BackendKey {
        self.backend
    }

    /// This connection's own cancel flag (§9.2).
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// The client sent `Terminate`, or a frame this surface refused fatally.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Set when this connection carried a `CancelRequest` instead of a
    /// startup: the backend the client wants stopped. The connection closes
    /// either way, and the server fires that backend's token.
    pub fn cancel_request(&self) -> Option<BackendKey> {
        self.cancel_request
    }

    /// Whether this session is listening on any channel (§9.3). A server
    /// that is idle on such a connection polls [`Connection::poll_notify`].
    pub fn is_listening(&self) -> bool {
        !self.listening.is_empty()
    }

    /// Drain the change feed and return the `NotificationResponse` bytes for
    /// whatever arrived, so a server can push notifications to an IDLE
    /// session. Empty when nothing arrived or nothing is listened to.
    pub fn poll_notify(&mut self) -> Vec<u8> {
        self.drain_changes();
        let mut out = Vec::new();
        self.write_notifications(&mut out);
        out
    }

    /// Feed freshly received bytes; returns the bytes to write back.
    ///
    /// Buffers partial frames, so any chunking is fine, and never panics on
    /// malformed input: it replies with an `ErrorResponse` and/or closes.
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        self.inbuf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut at = 0usize;

        while !self.closed {
            let buf = &self.inbuf[at..];
            if !self.started {
                if buf.len() < 4 {
                    break;
                }
                let len = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if len < 8 || len > f::MAX_STARTUP_LEN {
                    f::error_response(
                        &mut out,
                        types::PROTOCOL_VIOLATION,
                        "invalid startup packet",
                    );
                    self.closed = true;
                    break;
                }
                if buf.len() < len {
                    break;
                }
                let frame = buf[..len].to_vec();
                at += len;
                self.startup(&frame, &mut out);
            } else {
                if buf.len() < 5 {
                    break;
                }
                let len = i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                if len < 4 || len > f::MAX_MSG_LEN {
                    f::error_response(
                        &mut out,
                        types::PROTOCOL_VIOLATION,
                        "invalid message length",
                    );
                    self.closed = true;
                    break;
                }
                let total = 1 + len;
                if buf.len() < total {
                    break;
                }
                let typ = buf[0];
                let body = buf[5..total].to_vec();
                at += total;
                self.message(typ, &body, &mut out);
            }
        }

        if at > 0 {
            self.inbuf.drain(..at);
        }
        out
    }

    // ── startup ──────────────────────────────────────────────────────────

    fn startup(&mut self, frame: &[u8], out: &mut Vec<u8>) {
        let code = i32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
        match code {
            // §9.4: no TLS. `N` declines and the session continues in
            // plaintext, which is what every client does next.
            f::SSL_REQUEST | f::GSSAPI_REQUEST => out.push(b'N'),
            f::CANCEL_REQUEST => {
                // §9.2. The pair is 8 more bytes; anything shorter is a
                // malformed cancel and is dropped, as PostgreSQL drops it.
                if frame.len() >= 16 {
                    let pid = i32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]);
                    let secret = i32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]);
                    self.cancel_request = Some(BackendKey { pid, secret });
                }
                self.closed = true;
            }
            f::PROTOCOL_V3 => {
                // Startup parameters: `key\0value\0` pairs ending in an
                // empty key. `user` and `database` are accepted and ignored
                // (trust auth, one database per service); a
                // `statement_timeout` here is the same GUC §9.1 names.
                let mut reader = f::Reader::new(&frame[8..]);
                loop {
                    let key = reader.cstr();
                    if key.is_empty() {
                        break;
                    }
                    let value = reader.cstr();
                    if key.eq_ignore_ascii_case("statement_timeout") {
                        let _ = self.set_statement_timeout(&value);
                    }
                    self.gucs.insert(key.to_ascii_lowercase(), value);
                }
                self.banner(out);
                self.started = true;
            }
            other => {
                f::error_response(
                    out,
                    types::PROTOCOL_VIOLATION,
                    &format!("unsupported startup code {other}"),
                );
                self.closed = true;
            }
        }
    }

    /// Trust auth, the parameters a client reads before it will talk, the
    /// `(pid, secret)` pair §9.2 needs, and the first `ReadyForQuery`.
    fn banner(&mut self, out: &mut Vec<u8>) {
        f::authentication_ok(out);
        f::parameter_status(out, "server_version", SERVER_VERSION);
        f::parameter_status(out, "server_encoding", "UTF8");
        f::parameter_status(out, "client_encoding", "UTF8");
        f::parameter_status(out, "DateStyle", "ISO, MDY");
        f::parameter_status(out, "IntervalStyle", "postgres");
        f::parameter_status(out, "integer_datetimes", "on");
        f::parameter_status(out, "standard_conforming_strings", "on");
        f::parameter_status(out, "TimeZone", "UTC");
        f::parameter_status(out, "application_name", "");
        f::backend_key_data(out, self.backend.pid, self.backend.secret);
        f::ready_for_query(out, self.status());
    }

    fn status(&self) -> TransactionStatus {
        if self.txn_failed {
            TransactionStatus::Failed
        } else if self.txn.is_some() {
            TransactionStatus::InBlock
        } else {
            TransactionStatus::Idle
        }
    }

    // ── the message loop ─────────────────────────────────────────────────

    fn message(&mut self, typ: u8, body: &[u8], out: &mut Vec<u8>) {
        if self.skip_until_sync && !matches!(typ, f::F_SYNC | f::F_TERMINATE) {
            return;
        }
        match typ {
            f::F_QUERY => {
                let sql = f::cstr_at_front(body);
                self.simple_query(&sql, out);
                self.drain_changes();
                self.write_notifications(out);
                // A simple Query is its own unit of recovery: it ends with a
                // ReadyForQuery whatever happened, so the skip that an error
                // arms for the EXTENDED protocol's next Sync ends here.
                self.skip_until_sync = false;
                self.cancel.clear();
                f::ready_for_query(out, self.status());
            }
            f::F_PARSE => self.parse(body, out),
            f::F_BIND => self.bind(body, out),
            f::F_DESCRIBE => self.describe(body, out),
            f::F_EXECUTE => self.execute(body, out),
            f::F_CLOSE => self.close_message(body, out),
            f::F_SYNC => {
                self.skip_until_sync = false;
                self.drain_changes();
                self.write_notifications(out);
                // A `CancelRequest` cancels the statement IN FLIGHT and
                // nothing after it, which is PostgreSQL's rule: the backend
                // is idle at a `ReadyForQuery`, so a standing cancel ends
                // here. The service's own §4 handle is sticky by contract
                // and is NOT touched -- that one is the operator's.
                self.cancel.clear();
                f::ready_for_query(out, self.status());
            }
            // `Flush` asks for buffered output; `feed` already returns
            // everything it produced, so nothing is held back.
            f::F_FLUSH => {}
            f::F_TERMINATE => {
                self.finish();
                self.closed = true;
            }
            other => {
                let error = WireError::new(
                    types::PROTOCOL_VIOLATION,
                    format!("unsupported message type '{}'", other as char),
                );
                self.fail(out, &error);
            }
        }
    }

    /// Roll back an open transaction at `Terminate` or at a dropped socket.
    /// A close is not a commit (`OPS_CONTRACT` §1).
    fn finish(&mut self) {
        if let Some(mut guard) = self.txn.take() {
            let _ = guard.rollback();
        }
        self.txn_failed = false;
    }

    fn fail(&mut self, out: &mut Vec<u8>, error: &WireError) {
        f::error_response(out, error.sqlstate, &error.message);
        if self.txn.is_some() {
            self.txn_failed = true;
        }
        self.skip_until_sync = true;
    }

    // ── simple query ─────────────────────────────────────────────────────

    fn simple_query(&mut self, sql: &str, out: &mut Vec<u8>) {
        let statements = split_statements(sql);
        if statements.is_empty() {
            f::empty_query_response(out);
            return;
        }
        for statement in statements {
            // A simple Query has no row limit, so it streams: every page of
            // the walk goes straight out and nothing is held.
            match self.run_streaming(&statement, &[], &[0], true, out) {
                Ok(()) => {}
                // PostgreSQL abandons the rest of the string on the first
                // error.
                Err(error) => {
                    self.fail(out, &error);
                    return;
                }
            }
        }
    }

    // ── extended protocol ────────────────────────────────────────────────

    fn parse(&mut self, body: &[u8], out: &mut Vec<u8>) {
        let mut reader = f::Reader::new(body);
        let name = reader.cstr();
        let sql = reader.cstr();
        let count = reader.i16();
        if count < 0 || (count as usize).saturating_mul(4) > reader.remaining() {
            let error = WireError::new(
                types::PROTOCOL_VIOLATION,
                "invalid parameter type count in Parse",
            );
            self.fail(out, &error);
            return;
        }
        let param_oids: Vec<i32> = (0..count as usize).map(|_| reader.i32()).collect();
        // A syntax error belongs to `Parse`, not to the `Execute` three
        // messages later: a client that pipelines wants it here. A statement
        // this file answers itself is not put to the parser at all.
        if !self.is_session_statement(&sql) {
            if let Err(error) = sekejap_lang::parse_sql(sql.trim().trim_end_matches(';').trim()) {
                let wire = types::wire_error(&ServiceError::Sql(error));
                self.fail(out, &wire);
                return;
            }
        }
        self.statements.insert(name, Prepared { sql, param_oids });
        f::parse_complete(out);
    }

    fn bind(&mut self, body: &[u8], out: &mut Vec<u8>) {
        let mut reader = f::Reader::new(body);
        let portal_name = reader.cstr();
        let statement_name = reader.cstr();
        let Some(prepared) = self.statements.get(&statement_name).cloned() else {
            let error = WireError::new(
                types::INVALID_STATEMENT_NAME,
                format!("prepared statement \"{statement_name}\" does not exist"),
            );
            self.fail(out, &error);
            return;
        };

        // Every COUNT is checked against what the frame can hold before it
        // is believed: a two-byte `0xFFFF` is `-1`, and casting it straight
        // to `usize` is how a `Vec::with_capacity` becomes `usize::MAX`.
        let formats_count = reader.i16();
        if formats_count < 0 || (formats_count as usize).saturating_mul(2) > reader.remaining() {
            let error = WireError::new(
                types::PROTOCOL_VIOLATION,
                "invalid parameter format code count in Bind",
            );
            self.fail(out, &error);
            return;
        }
        let param_formats: Vec<i16> = (0..formats_count as usize).map(|_| reader.i16()).collect();

        let params_count = reader.i16();
        if params_count < 0 || (params_count as usize).saturating_mul(4) > reader.remaining() {
            let error =
                WireError::new(types::PROTOCOL_VIOLATION, "invalid parameter count in Bind");
            self.fail(out, &error);
            return;
        }
        let mut params = Vec::with_capacity(params_count as usize);
        for at in 0..params_count as usize {
            let len = reader.i32();
            let bytes = if len < 0 {
                None
            } else {
                Some(reader.bytes(len as usize).to_vec())
            };
            let format = format_at(&param_formats, at);
            let type_oid = prepared.param_oids.get(at).copied().unwrap_or(0);
            match types::decode_param(bytes.as_deref(), type_oid, format) {
                Ok(param) => params.push(param),
                Err(error) => {
                    let wire = types::wire_error(&ServiceError::Sql(error));
                    self.fail(out, &wire);
                    return;
                }
            }
        }

        let result_count = reader.i16();
        if result_count < 0 || (result_count as usize).saturating_mul(2) > reader.remaining() {
            let error = WireError::new(
                types::PROTOCOL_VIOLATION,
                "invalid result format code count in Bind",
            );
            self.fail(out, &error);
            return;
        }
        let result_formats: Vec<i16> = (0..result_count as usize).map(|_| reader.i16()).collect();

        self.portals.insert(
            portal_name,
            Portal {
                sql: prepared.sql,
                params,
                result_formats,
                answer: None,
                tag: None,
                delivered: 0,
                executed: false,
            },
        );
        f::bind_complete(out);
    }

    fn describe(&mut self, body: &[u8], out: &mut Vec<u8>) {
        let mut reader = f::Reader::new(body);
        let kind = reader.byte();
        let name = reader.cstr();
        if kind == b'S' {
            let Some(prepared) = self.statements.get(&name).cloned() else {
                let error = WireError::new(
                    types::INVALID_STATEMENT_NAME,
                    format!("prepared statement \"{name}\" does not exist"),
                );
                self.fail(out, &error);
                return;
            };
            // The `$n` OIDs: what the `Parse` declared, and `text` for every
            // position it left undeclared. `text` rather than `unknown`
            // (705) because it is a real type whose value maps onto
            // `Param::Text` with nothing inferred.
            let count = count_parameters(&prepared.sql).max(prepared.param_oids.len());
            let oids: Vec<i32> = (0..count)
                .map(|at| match prepared.param_oids.get(at).copied() {
                    Some(0) | None => oid::TEXT,
                    Some(declared) => declared,
                })
                .collect();
            f::parameter_description(out, &oids);
            match self.describe_columns(&prepared.sql, &oids) {
                Some(fields) => f::row_description(out, &fields),
                None => f::no_data(out),
            }
            return;
        }
        // A portal. Running it now is what lets its columns be described,
        // and the run is held so `Execute` does not repeat it.
        match self.portal_answer(&name, out) {
            Ok(Some(fields)) => f::row_description(out, &fields),
            Ok(None) => f::no_data(out),
            Err(error) => self.fail(out, &error),
        }
    }

    fn execute(&mut self, body: &[u8], out: &mut Vec<u8>) {
        let mut reader = f::Reader::new(body);
        let name = reader.cstr();
        let max_rows = reader.i32().max(0) as usize;

        let Some(portal) = self.portals.get(&name) else {
            let error = WireError::new(
                types::INVALID_CURSOR_NAME,
                format!("portal \"{name}\" does not exist"),
            );
            self.fail(out, &error);
            return;
        };

        // An `Execute` with NO row limit on a portal that has not been run
        // streams: the walk's pages go straight out and nothing is held.
        // `Execute` never sends a `RowDescription` -- the client already has
        // one from `Describe`.
        if max_rows == 0 && !portal.executed {
            let sql = portal.sql.clone();
            let params = portal.params.clone();
            let formats = portal.result_formats.clone();
            match self.run_streaming(&sql, &params, &formats, false, out) {
                Ok(()) => {
                    if let Some(portal) = self.portals.get_mut(&name) {
                        portal.executed = true;
                    }
                }
                Err(error) => self.fail(out, &error),
            }
            return;
        }

        // Otherwise the answer is held, and this `Execute` hands out a slice.
        if let Err(error) = self.portal_answer(&name, out) {
            self.fail(out, &error);
            return;
        }
        let Some(portal) = self.portals.get_mut(&name) else {
            return;
        };
        match &portal.answer {
            Some(answer) => {
                let start = portal.delivered;
                let end = if max_rows == 0 {
                    answer.rows.len()
                } else {
                    (start + max_rows).min(answer.rows.len())
                };
                for row in &answer.rows[start..end] {
                    emit_row(out, row, &answer.fields);
                }
                portal.delivered = end;
                if end < answer.rows.len() {
                    f::portal_suspended(out);
                } else {
                    let tag = answer
                        .tag
                        .clone()
                        .unwrap_or_else(|| format!("SELECT {}", end - start));
                    f::command_complete(out, &tag);
                }
            }
            None => match portal.tag.clone() {
                Some(tag) => f::command_complete(out, &tag),
                None => f::empty_query_response(out),
            },
        }
    }

    fn close_message(&mut self, body: &[u8], out: &mut Vec<u8>) {
        let mut reader = f::Reader::new(body);
        let kind = reader.byte();
        let name = reader.cstr();
        if kind == b'S' {
            self.statements.remove(&name);
        } else {
            self.portals.remove(&name);
        }
        f::close_complete(out);
    }

    // ── running one statement ────────────────────────────────────────────

    /// Run a statement whose rows go STRAIGHT out, page by page.
    ///
    /// `describe` says whether a `RowDescription` precedes them: the simple
    /// protocol sends one, `Execute` never does.
    fn run_streaming(
        &mut self,
        sql: &str,
        params: &[Param],
        formats: &[i16],
        describe: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), WireError> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            f::empty_query_response(out);
            return Ok(());
        }
        if let Some(outcome) = self.session_statement(trimmed, params, out)? {
            emit_outcome(out, outcome, formats, describe);
            return Ok(());
        }
        if is_read(trimmed) {
            let mut rendered: Vec<u8> = Vec::new();
            let mut count = 0u64;
            let fields = self.walk(trimmed, params, &mut |row, fields| {
                emit_row(&mut rendered, &row.values, fields);
                count += 1;
                Ok(())
            }, formats)?;
            if describe {
                f::row_description(out, &fields);
            }
            out.extend_from_slice(&rendered);
            f::command_complete(out, &format!("SELECT {count}"));
            return Ok(());
        }
        let outcome = self.write_statement(trimmed, params, out)?;
        emit_outcome(out, outcome, formats, describe);
        Ok(())
    }

    /// Ensure a portal has been run and its rows held; returns the column
    /// descriptions when it returns rows.
    fn portal_answer(
        &mut self,
        name: &str,
        out: &mut Vec<u8>,
    ) -> Result<Option<Vec<FieldDescription>>, WireError> {
        let Some(portal) = self.portals.get(name) else {
            return Err(WireError::new(
                types::INVALID_CURSOR_NAME,
                format!("portal \"{name}\" does not exist"),
            ));
        };
        if portal.executed {
            return Ok(portal.answer.as_ref().map(|answer| answer.fields.clone()));
        }
        let sql = portal.sql.clone();
        let params = portal.params.clone();
        let formats = portal.result_formats.clone();
        let outcome = self.run_buffered(&sql, &params, &formats, out)?;
        let portal = self.portals.get_mut(name).expect("checked above");
        portal.executed = true;
        match outcome {
            Outcome::Rows(mut answer) => {
                answer.fields = apply_formats(&answer.fields, &formats);
                let fields = answer.fields.clone();
                portal.answer = Some(answer);
                Ok(Some(fields))
            }
            Outcome::Command(tag) => {
                portal.tag = Some(tag);
                Ok(None)
            }
            Outcome::Empty => Ok(None),
        }
    }

    /// Run a statement and MATERIALISE its answer, under the two ceilings a
    /// suspended portal or an open cursor is bounded by.
    fn run_buffered(
        &mut self,
        sql: &str,
        params: &[Param],
        formats: &[i16],
        out: &mut Vec<u8>,
    ) -> Result<Outcome, WireError> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return Ok(Outcome::Empty);
        }
        if let Some(outcome) = self.session_statement(trimmed, params, out)? {
            return Ok(outcome);
        }
        if is_read(trimmed) {
            let mut rows: Vec<Vec<SqlValue>> = Vec::new();
            let mut bytes = 0usize;
            let fields = self.walk(trimmed, params, &mut |row, _| {
                if rows.len() >= CURSOR_ROW_CAP {
                    return Err(SqlError::Unsupported(format!(
                        "a HELD answer is bounded at {CURSOR_ROW_CAP} rows and this one reached \
                         it. e4's prepared query borrows the database handle for the life of the \
                         walk and has no resume cursor a protocol message could carry, so a \
                         suspended portal or an open cursor HOLDS the rows it has not handed out. \
                         Run the statement with no row limit, which streams and holds nothing, or \
                         add a LIMIT"
                    )));
                }
                bytes += row_bytes(&row.values);
                if bytes > CURSOR_BYTES_CAP {
                    return Err(SqlError::Unsupported(format!(
                        "a HELD answer is bounded at {CURSOR_BYTES_CAP} bytes and this one \
                         reached it. Run the statement with no row limit, which streams and holds \
                         nothing, or project fewer columns"
                    )));
                }
                rows.push(row.values.clone());
                Ok(())
            }, formats)?;
            return Ok(Outcome::Rows(Answer {
                fields,
                rows,
                tag: None,
            }));
        }
        self.write_statement(trimmed, params, out)
    }

    /// Page a read statement, handing each assembled row to `body`.
    ///
    /// Returns the column descriptions, which are answered from the SOURCE
    /// COLLECTION's declared types and NOT from the data -- see
    /// [`field_descriptions`] -- with the `Bind`'s result format codes
    /// stamped on, because a cell is encoded in the format its column was
    /// described with.
    fn walk(
        &mut self,
        sql: &str,
        params: &[Param],
        body: &mut dyn FnMut(&SqlRow, &[FieldDescription]) -> Result<(), SqlError>,
        formats: &[i16],
    ) -> Result<Vec<FieldDescription>, WireError> {
        let budget = self.budget();
        let cancel = self.cancel.clone();
        let interrupt = self.service.interrupt_handle();
        self.refresh_reader()?;
        let snapshot = self.reader.as_ref().expect("refresh_reader mints one");

        snapshot.with(|db| {
            let db: &Database = db;
            let mut cancelled = || cancel.is_cancelled() || interrupt.is_cancelled();
            let prepared = sekejap_lang::prepare_sql_with(db, sql, params, budget, &mut cancelled)
                .map_err(|e| types::wire_error(&ServiceError::Sql(e)))?;

            if prepared.is_select() || prepared.is_aggregate() {
                let fields = apply_formats(&field_descriptions(db, &prepared), formats);
                let paged = if prepared.is_aggregate() {
                    prepared.for_each_group_with(db, PAGE_ROWS, budget, &mut cancelled, &mut |row| {
                        body(row, &fields)
                    })
                } else {
                    prepared.for_each_row_with(db, PAGE_ROWS, budget, &mut cancelled, &mut |row| {
                        body(row, &fields)
                    })
                };
                paged.map_err(|e| types::wire_error(&ServiceError::Sql(e)))?;
                return Ok(fields);
            }

            // EXPLAIN and the notice families: one answer, not a walk.
            match prepared.run(db) {
                Ok(SqlResult::Rows { columns, rows }) => {
                    let fields = apply_formats(
                        &columns.iter().map(|name| text_field(name)).collect::<Vec<_>>(),
                        formats,
                    );
                    for row in &rows {
                        body(row, &fields)
                            .map_err(|e| types::wire_error(&ServiceError::Sql(e)))?;
                    }
                    Ok(fields)
                }
                Ok(SqlResult::Explain(text)) | Ok(SqlResult::Notice(text)) => {
                    let fields = apply_formats(&[text_field("QUERY PLAN")], formats);
                    let row = SqlRow {
                        id: EntityId {
                            collection: CollectionId(0),
                            sequence: 0,
                        },
                        values: vec![SqlValue::Text(text)],
                    };
                    body(&row, &fields).map_err(|e| types::wire_error(&ServiceError::Sql(e)))?;
                    Ok(fields)
                }
                Ok(SqlResult::Affected(_)) => Ok(Vec::new()),
                Err(e) => Err(types::wire_error(&ServiceError::Sql(e))),
            }
        })
    }

    /// Run a statement through the service's single WRITER, committing it
    /// unless a `BEGIN` block is open.
    fn write_statement(
        &mut self,
        sql: &str,
        params: &[Param],
        out: &mut Vec<u8>,
    ) -> Result<Outcome, WireError> {
        if self.txn_failed {
            return Err(WireError::new(
                "25P02",
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        let result = if self.txn.is_some() {
            let guard = self.txn.as_mut().expect("checked above");
            guard.sql(sql, params)
        } else {
            match self.service.try_writer() {
                Ok(mut guard) => match guard.sql(sql, params) {
                    Ok(result) => guard.commit().map(|()| result),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            }
        };
        let result = result.map_err(|e| types::wire_error(&e))?;
        Ok(match result {
            SqlResult::Affected(rows) => Outcome::Command(command_tag(sql, rows)),
            SqlResult::Notice(text) => {
                f::notice_response(out, "00000", &text);
                Outcome::Command(command_tag(sql, 0))
            }
            SqlResult::Explain(text) => Outcome::Rows(Answer {
                fields: vec![text_field("QUERY PLAN")],
                rows: vec![vec![SqlValue::Text(text)]],
                tag: None,
            }),
            SqlResult::Rows { columns, rows } => Outcome::Rows(Answer {
                fields: columns.iter().map(|name| text_field(name)).collect(),
                rows: rows.into_iter().map(|row| row.values).collect(),
                tag: None,
            }),
        })
    }

    // ── the read view ────────────────────────────────────────────────────

    /// Mint this connection's private read handle, or keep the one it has
    /// when the service has published nothing since.
    fn refresh_reader(&mut self) -> Result<(), WireError> {
        let published = self.service.reader().serial();
        if self.reader.is_none() || self.reader_at != published {
            // Drop the old handle FIRST, so a connection never holds two
            // reader slots at once (`OPS_CONTRACT` §1 bounds them).
            self.reader = None;
            let snapshot = self
                .service
                .open_reader()
                .map_err(|e| types::wire_error(&e))?;
            self.reader = Some(snapshot);
            self.reader_at = published;
        }
        Ok(())
    }

    /// §3: this statement's budget. A `statement_timeout` set on this
    /// session reached [`ServiceDatabase::set_statement_timeout`], so the
    /// service's own bound IS this session's.
    fn budget(&self) -> QueryBudget {
        self.service.bound(QueryBudget::unlimited())
    }

    // ── the session surface ──────────────────────────────────────────────

    /// True for a statement this file answers itself.
    fn is_session_statement(&self, sql: &str) -> bool {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        let first = word(trimmed, 0);
        if matches!(
            first.as_str(),
            "BEGIN"
                | "START"
                | "COMMIT"
                | "END"
                | "ROLLBACK"
                | "ABORT"
                | "RESET"
                | "DISCARD"
                | "LISTEN"
                | "UNLISTEN"
                | "NOTIFY"
                | "DECLARE"
                | "FETCH"
                | "MOVE"
                | "CLOSE"
        ) {
            return true;
        }
        if first == "SET" {
            return true;
        }
        if first == "SHOW" {
            return self.show_target(trimmed).is_some();
        }
        self.fixed_select(&trimmed.to_ascii_uppercase()).is_some()
    }

    /// The statements this file answers itself: transaction control, the
    /// GUCs, `LISTEN`/`NOTIFY`, the cursors, and the fixed rows a client
    /// sends before it will talk. `None` means "hand it to the engine".
    fn session_statement(
        &mut self,
        sql: &str,
        params: &[Param],
        out: &mut Vec<u8>,
    ) -> Result<Option<Outcome>, WireError> {
        let first = word(sql, 0);
        let upper = sql.to_ascii_uppercase();

        // A catalog query is an ordinary statement: `sekejap_lang` answers the
        // `pg_catalog` / `information_schema` views as virtual rows
        // (docs/dist/PG_SURFACE.md) and refuses by name what it does not have.
        match first.as_str() {
            "BEGIN" | "START" => {
                if self.txn.is_some() {
                    return Ok(Some(Outcome::Command("BEGIN".to_owned())));
                }
                let guard = self
                    .service
                    .try_writer()
                    .map_err(|e| types::wire_error(&e))?;
                self.txn = Some(guard);
                self.txn_failed = false;
                Ok(Some(Outcome::Command("BEGIN".to_owned())))
            }
            "COMMIT" | "END" => {
                if let Some(mut guard) = self.txn.take() {
                    if self.txn_failed {
                        let _ = guard.rollback();
                        self.txn_failed = false;
                        return Ok(Some(Outcome::Command("ROLLBACK".to_owned())));
                    }
                    guard.commit().map_err(|e| types::wire_error(&e))?;
                }
                self.txn_failed = false;
                Ok(Some(Outcome::Command("COMMIT".to_owned())))
            }
            "ROLLBACK" | "ABORT" => {
                if let Some(mut guard) = self.txn.take() {
                    guard.rollback().map_err(|e| types::wire_error(&e))?;
                }
                self.txn_failed = false;
                Ok(Some(Outcome::Command("ROLLBACK".to_owned())))
            }
            "SET" => self.set_statement(sql),
            "RESET" => {
                let name = word(sql, 1).to_ascii_lowercase();
                if name == "statement_timeout" || name == "all" {
                    self.service.clear_statement_timeout();
                }
                self.gucs.remove(&name);
                Ok(Some(Outcome::Command("RESET".to_owned())))
            }
            "DISCARD" => {
                self.portals.clear();
                self.cursors.clear();
                self.statements.clear();
                Ok(Some(Outcome::Command("DISCARD ALL".to_owned())))
            }
            "LISTEN" => {
                let channel = unquote(&word(sql, 1)).to_ascii_lowercase();
                if channel.is_empty() {
                    return Err(WireError::new(types::SYNTAX_ERROR, "LISTEN needs a channel"));
                }
                if self.changes.is_none() {
                    self.changes = Some(self.service.subscribe_changes());
                }
                if !self.listening.iter().any(|c| *c == channel) {
                    self.listening.push(channel);
                }
                Ok(Some(Outcome::Command("LISTEN".to_owned())))
            }
            "UNLISTEN" => {
                let channel = unquote(&word(sql, 1)).to_ascii_lowercase();
                if channel == "*" {
                    self.listening.clear();
                } else {
                    self.listening.retain(|c| *c != channel);
                }
                if self.listening.is_empty() {
                    if let Some(receiver) = self.changes.take() {
                        self.service.unsubscribe(receiver.id());
                    }
                }
                Ok(Some(Outcome::Command("UNLISTEN".to_owned())))
            }
            "NOTIFY" => Err(WireError::new(types::FEATURE_NOT_SUPPORTED, NOTIFY_REFUSAL)),
            "DECLARE" => self.declare_cursor(sql, params, out).map(Some),
            "FETCH" => self.fetch_cursor(sql).map(Some),
            "MOVE" => self.move_cursor(sql).map(Some),
            "CLOSE" => self.close_cursor(sql).map(Some),
            "SHOW" => Ok(self.show_target(sql)),
            "SELECT" => Ok(self.fixed_select(&upper)),
            _ => Ok(None),
        }
    }

    /// `SET [SESSION|LOCAL] <name> {=|TO} <value>`.
    ///
    /// §9.1: `statement_timeout` is routed to
    /// [`ServiceDatabase::set_statement_timeout`]. Every other client GUC is
    /// remembered so `SHOW` prints it back, and a knob the SQL layer owns
    /// (`ef_search`, `diskann.*`) falls through to it.
    fn set_statement(&mut self, sql: &str) -> Result<Option<Outcome>, WireError> {
        let mut at = 1;
        let mut name = word(sql, at);
        if name == "LOCAL" || name == "SESSION" {
            at += 1;
            name = word(sql, at);
        }
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("ef_search")
            || lower.starts_with("hnsw.")
            || lower.starts_with("diskann.")
            || lower.starts_with("query_search_list_size")
            || lower.starts_with("query_rescore")
        {
            return Ok(None); // the SQL layer's own knob
        }
        if lower == "time" && word(sql, at + 1) == "ZONE" {
            self.gucs
                .insert("timezone".to_owned(), unquote(&word(sql, at + 2)));
            return Ok(Some(Outcome::Command("SET".to_owned())));
        }
        let value = unquote(set_value(sql));
        if lower == "statement_timeout" {
            self.set_statement_timeout(&value)?;
        }
        self.gucs.insert(lower, value);
        Ok(Some(Outcome::Command("SET".to_owned())))
    }

    /// PostgreSQL's `statement_timeout` spelling: bare milliseconds, or a
    /// number with a unit. `0` is no limit, which is §3's `None`.
    fn set_statement_timeout(&mut self, value: &str) -> Result<(), WireError> {
        let text = value.trim();
        if text.eq_ignore_ascii_case("default") {
            self.service.clear_statement_timeout();
            return Ok(());
        }
        let micros = parse_timeout_micros(text).ok_or_else(|| {
            WireError::new(
                types::INVALID_PARAMETER,
                format!(
                    "statement_timeout: `{text}` is not milliseconds or an interval \
                     (`250ms`, `5s`, `2min`); 0 means no limit"
                ),
            )
        })?;
        if micros == 0 {
            self.service.clear_statement_timeout();
        } else {
            self.service
                .set_statement_timeout(Duration::from_micros(micros));
        }
        Ok(())
    }

    /// `SHOW <guc>`. The sekejap `SHOW` targets return `None` and fall
    /// through to the engine, which refuses them by name while they are T2.
    fn show_target(&self, sql: &str) -> Option<Outcome> {
        let name = word(sql, 1);
        let lower = name.to_ascii_lowercase();
        if lower.is_empty()
            || matches!(
                lower.as_str(),
                "tables"
                    | "edges"
                    | "index"
                    | "indexes"
                    | "collections"
                    | "status"
                    | "storage"
                    | "create"
                    | "all"
            )
        {
            return None;
        }
        if lower == "statement_timeout" {
            let text = match self.service.statement_timeout() {
                None => "0".to_owned(),
                Some(timeout) => format!("{}ms", timeout.as_millis()),
            };
            return Some(Outcome::Rows(one_cell("statement_timeout", &text)));
        }
        let value = self.gucs.get(&lower).cloned().unwrap_or_default();
        Some(Outcome::Rows(one_cell(&lower, &value)))
    }

    /// The fixed rows a client sends before it will talk: `version()` and
    /// the `current_*` functions. `QL_CONTRACT` §2 places these in T2 as
    /// `p3-pg-surface`; they are answered here because a connection that
    /// cannot answer them never reaches a statement at all, and each is a
    /// constant rather than a query.
    fn fixed_select(&self, upper: &str) -> Option<Outcome> {
        let body = upper.trim().trim_end_matches(';').trim();
        if !body.starts_with("SELECT") || body.contains(" FROM ") {
            return None;
        }
        if body.starts_with("SELECT VERSION()") {
            return Some(Outcome::Rows(one_cell(
                "version",
                &format!(
                    "PostgreSQL {SERVER_VERSION} (sekejap {}) on {}, 64-bit",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::ARCH
                ),
            )));
        }
        if body.contains("CURRENT_SCHEMA") {
            return Some(Outcome::Rows(one_cell("current_schema", "public")));
        }
        if body.contains("CURRENT_DATABASE") {
            return Some(Outcome::Rows(one_cell("current_database", "sekejap")));
        }
        if body.contains("CURRENT_USER") || body.contains("SESSION_USER") {
            return Some(Outcome::Rows(one_cell("current_user", "sekejap")));
        }
        None
    }

    // ── cursors (QL_CONTRACT §2, T2 -> T1 in this session) ───────────────

    /// `DECLARE <name> [BINARY] [NO SCROLL] CURSOR [WITH|WITHOUT HOLD] FOR
    /// <select>`.
    fn declare_cursor(
        &mut self,
        sql: &str,
        params: &[Param],
        out: &mut Vec<u8>,
    ) -> Result<Outcome, WireError> {
        let name = unquote(&word(sql, 1)).to_ascii_lowercase();
        if name.is_empty() {
            return Err(WireError::new(types::SYNTAX_ERROR, "DECLARE needs a name"));
        }
        let upper = sql.to_ascii_uppercase();
        let Some(at) = upper.find(" FOR ") else {
            return Err(WireError::new(
                types::SYNTAX_ERROR,
                "DECLARE <name> CURSOR FOR <select>",
            ));
        };
        let body = sql[at + 5..].trim().to_owned();
        if !is_read(&body) {
            return Err(WireError::new(
                types::FEATURE_NOT_SUPPORTED,
                "DECLARE ... CURSOR FOR takes a SELECT: a cursor is pages over a prepared query \
                 (QL_CONTRACT §2), and a write has no pages to hold open",
            ));
        }
        let outcome = self.run_buffered(&body, params, &[0], out)?;
        let Outcome::Rows(answer) = outcome else {
            return Err(WireError::new(
                types::FEATURE_NOT_SUPPORTED,
                "DECLARE ... CURSOR FOR takes a statement that returns rows",
            ));
        };
        self.cursors.insert(
            name,
            Cursor {
                answer,
                delivered: 0,
            },
        );
        Ok(Outcome::Command("DECLARE CURSOR".to_owned()))
    }

    /// `FETCH [FORWARD] [n|ALL|NEXT] [FROM|IN] <name>`.
    fn fetch_cursor(&mut self, sql: &str) -> Result<Outcome, WireError> {
        let (count, name) = fetch_arguments(sql);
        let Some(cursor) = self.cursors.get_mut(&name) else {
            return Err(WireError::new(
                types::INVALID_CURSOR_NAME,
                format!("cursor \"{name}\" does not exist"),
            ));
        };
        let start = cursor.delivered;
        let end = match count {
            Some(n) => (start + n).min(cursor.answer.rows.len()),
            None => cursor.answer.rows.len(),
        };
        cursor.delivered = end;
        Ok(Outcome::Rows(Answer {
            fields: cursor.answer.fields.clone(),
            rows: cursor.answer.rows[start..end].to_vec(),
            tag: Some(format!("FETCH {}", end - start)),
        }))
    }

    /// `MOVE [FORWARD] [n|ALL] [FROM|IN] <name>`: a `FETCH` whose rows are
    /// discarded. The same held answer, so it costs no walk.
    fn move_cursor(&mut self, sql: &str) -> Result<Outcome, WireError> {
        let (count, name) = fetch_arguments(sql);
        let Some(cursor) = self.cursors.get_mut(&name) else {
            return Err(WireError::new(
                types::INVALID_CURSOR_NAME,
                format!("cursor \"{name}\" does not exist"),
            ));
        };
        let start = cursor.delivered;
        let end = match count {
            Some(n) => (start + n).min(cursor.answer.rows.len()),
            None => cursor.answer.rows.len(),
        };
        cursor.delivered = end;
        Ok(Outcome::Command(format!("MOVE {}", end - start)))
    }

    /// `CLOSE <name>` or `CLOSE ALL`.
    fn close_cursor(&mut self, sql: &str) -> Result<Outcome, WireError> {
        let name = unquote(&word(sql, 1)).to_ascii_lowercase();
        if name == "all" {
            self.cursors.clear();
            return Ok(Outcome::Command("CLOSE CURSOR".to_owned()));
        }
        if self.cursors.remove(&name).is_none() {
            return Err(WireError::new(
                types::INVALID_CURSOR_NAME,
                format!("cursor \"{name}\" does not exist"),
            ));
        }
        Ok(Outcome::Command("CLOSE CURSOR".to_owned()))
    }

    // ── describe without running (extended protocol) ─────────────────────

    /// The columns a statement will return, answered WITHOUT running it.
    ///
    /// `rust-postgres` reads this at `Describe('S')`, before any parameter
    /// is bound, and decodes every later row with what it reads here -- so
    /// the answer must not depend on the data, and it does not: a column
    /// that is a declared FIELD of the source collection is typed from the
    /// catalog, and every computed column is `text`.
    ///
    /// The statement is COMPILED with a probe parameter per `$n`, built from
    /// the OID the `Parse` declared, because a plan is what names the
    /// collection. A probe that does not compile returns `None`, and the
    /// client is told `NoData` rather than a guess.
    fn describe_columns(&mut self, sql: &str, oids: &[i32]) -> Option<Vec<FieldDescription>> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if self.is_session_statement(trimmed) || !is_read(trimmed) {
            return None;
        }
        let probe: Vec<Param> = oids.iter().map(|oid| probe_param(*oid)).collect();
        let budget = self.budget();
        self.refresh_reader().ok()?;
        let snapshot = self.reader.as_ref()?;
        snapshot.with(|db| {
            let db: &Database = db;
            let prepared =
                sekejap_lang::prepare_sql_with(db, trimmed, &probe, budget, &mut || false).ok()?;
            if prepared.is_select() || prepared.is_aggregate() {
                Some(field_descriptions(db, &prepared))
            } else {
                Some(vec![text_field("QUERY PLAN")])
            }
        })
    }

    // ── §9.3 notifications ───────────────────────────────────────────────

    /// Drain the change feed into this session's pending notifications: one
    /// per listening channel per committed batch.
    fn drain_changes(&mut self) {
        if self.listening.is_empty() {
            return;
        }
        let Some(receiver) = &self.changes else {
            return;
        };
        let mut events = Vec::new();
        while let Some(event) = receiver.try_recv() {
            events.push(event);
        }
        for event in events {
            let payload = notification_payload(&event);
            for channel in &self.listening {
                self.pending_notifications
                    .push((channel.clone(), payload.clone()));
            }
        }
    }

    fn write_notifications(&mut self, out: &mut Vec<u8>) {
        let pid = self.backend.pid;
        for (channel, payload) in self.pending_notifications.drain(..) {
            f::notification_response(out, pid, &channel, &payload);
        }
    }
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        // A dropped socket is not a commit: an open transaction rolls back.
        self.finish();
        if let Some(receiver) = self.changes.take() {
            self.service.unsubscribe(receiver.id());
        }
    }
}

// ── the answer, framed ───────────────────────────────────────────────────

fn emit_outcome(out: &mut Vec<u8>, outcome: Outcome, formats: &[i16], describe: bool) {
    match outcome {
        Outcome::Rows(answer) => {
            let fields = apply_formats(&answer.fields, formats);
            if describe {
                f::row_description(out, &fields);
            }
            for row in &answer.rows {
                emit_row(out, row, &fields);
            }
            let tag = answer
                .tag
                .unwrap_or_else(|| format!("SELECT {}", answer.rows.len()));
            f::command_complete(out, &tag);
        }
        Outcome::Command(tag) => f::command_complete(out, &tag),
        Outcome::Empty => f::empty_query_response(out),
    }
}

fn emit_row(out: &mut Vec<u8>, values: &[SqlValue], fields: &[FieldDescription]) {
    let cells: Vec<Option<Vec<u8>>> = values
        .iter()
        .enumerate()
        .map(|(at, value)| {
            let field = fields.get(at);
            let type_oid = field.map_or(oid::TEXT, |field| field.type_oid);
            let format = field.map_or(0, |field| field.format);
            types::encode_cell(value, type_oid, format)
        })
        .collect();
    f::data_row(out, &cells);
}

/// Stamp the `Bind`'s result format codes onto the column descriptions. An
/// empty list is all text; one code is that code for every column.
fn apply_formats(fields: &[FieldDescription], formats: &[i16]) -> Vec<FieldDescription> {
    fields
        .iter()
        .enumerate()
        .map(|(at, field)| FieldDescription {
            format: format_at(formats, at),
            ..field.clone()
        })
        .collect()
}

fn format_at(formats: &[i16], at: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(at).copied().unwrap_or(0),
    }
}

/// The column descriptions for a compiled statement, from the SOURCE
/// COLLECTION's declared types: the declared spelling first (`TIMESTAMPTZ`
/// and `DATE` are both `Kind::Int`, so only the catalog says which), then
/// the stored `Kind`, then `text`.
///
/// Data-independent on purpose. `Describe('S')` is answered before a row is
/// walked and a client decodes every later row with what it read there, so
/// an OID inferred from VALUES would type the same statement differently at
/// `Describe` and at `Execute`. A computed column -- an aggregate, a row
/// function, a literal, `_id` -- is therefore `text`, and its value is the
/// text `sekejap_lang` prints.
fn field_descriptions(db: &Database, prepared: &PreparedSql) -> Vec<FieldDescription> {
    let info = prepared
        .source_collection()
        .and_then(|id| db.collection_info(id).ok());
    prepared
        .columns()
        .iter()
        .map(|name| {
            let type_oid = info
                .as_ref()
                .and_then(|info| {
                    info.declared
                        .iter()
                        .find(|(field, _)| field == name)
                        .and_then(|(_, declared)| types::oid_for_declared(declared))
                        .or_else(|| {
                            info.layout
                                .fields
                                .iter()
                                .find(|(field, _)| field == name)
                                .map(|(_, kind)| types::oid_for_kind(kind))
                        })
                })
                .unwrap_or(oid::TEXT);
            FieldDescription {
                name: name.clone(),
                type_oid,
                type_size: types::type_size(type_oid),
                format: 0,
            }
        })
        .collect()
}

fn text_field(name: &str) -> FieldDescription {
    FieldDescription {
        name: name.to_owned(),
        type_oid: oid::TEXT,
        type_size: -1,
        format: 0,
    }
}

fn one_cell(column: &str, value: &str) -> Answer {
    Answer {
        fields: vec![text_field(column)],
        rows: vec![vec![SqlValue::Text(value.to_owned())]],
        tag: None,
    }
}

/// What one held row costs the buffer, for [`CURSOR_BYTES_CAP`].
fn row_bytes(values: &[SqlValue]) -> usize {
    values
        .iter()
        .map(|value| match value {
            SqlValue::Text(text) => text.len() + 8,
            SqlValue::Json(value) => value.to_string().len() + 8,
            _ => 16,
        })
        .sum()
}

// ── statement shapes ─────────────────────────────────────────────────────

/// True for a statement served from a snapshot rather than from the writer.
fn is_read(sql: &str) -> bool {
    matches!(
        word(sql, 0).as_str(),
        "SELECT" | "TABLE" | "VALUES" | "WITH" | "EXPLAIN" | "SHOW"
    )
}

/// The `n`th whitespace-separated word, upper-cased.
fn word(sql: &str, n: usize) -> String {
    sql.trim()
        .split_whitespace()
        .nth(n)
        .unwrap_or("")
        .trim_end_matches(';')
        .to_ascii_uppercase()
}

/// Everything after the first `=` or ` TO ` of a `SET`.
fn set_value(sql: &str) -> &str {
    if let Some(at) = sql.find('=') {
        return sql[at + 1..].trim().trim_end_matches(';').trim();
    }
    let upper = sql.to_ascii_uppercase();
    if let Some(at) = upper.find(" TO ") {
        return sql[at + 4..].trim().trim_end_matches(';').trim();
    }
    ""
}

fn unquote(text: &str) -> String {
    let trimmed = text.trim().trim_end_matches(';').trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"'))
    {
        return trimmed[1..trimmed.len() - 1].to_owned();
    }
    trimmed.to_owned()
}

/// `FETCH`/`MOVE` arguments: the count (`None` for `ALL`) and the cursor
/// name, lower-cased.
fn fetch_arguments(sql: &str) -> (Option<usize>, String) {
    let mut words: Vec<String> = sql
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .map(|word| word.trim_end_matches(';').to_owned())
        .collect();
    if !words.is_empty() {
        words.remove(0); // FETCH / MOVE
    }
    let mut count = Some(1usize);
    let mut at = 0usize;
    while at < words.len() {
        let upper = words[at].to_ascii_uppercase();
        match upper.as_str() {
            "FORWARD" | "NEXT" | "RELATIVE" | "ABSOLUTE" => at += 1,
            "ALL" => {
                count = None;
                at += 1;
            }
            "FROM" | "IN" => {
                at += 1;
                break;
            }
            _ => match upper.parse::<usize>() {
                Ok(n) => {
                    count = Some(n);
                    at += 1;
                }
                Err(_) => break,
            },
        }
    }
    let name = words
        .get(at)
        .map(|word| unquote(word).to_ascii_lowercase())
        .unwrap_or_default();
    (count, name)
}

/// The `CommandComplete` tag for a statement that wrote `rows` rows.
fn command_tag(sql: &str, rows: u64) -> String {
    let first = word(sql, 0);
    match first.as_str() {
        "INSERT" => format!("INSERT 0 {rows}"),
        "UPDATE" => format!("UPDATE {rows}"),
        "DELETE" => format!("DELETE {rows}"),
        "CREATE" | "DROP" | "ALTER" | "REINDEX" | "TRUNCATE" => {
            let second = word(sql, 1);
            if second.is_empty() {
                first
            } else {
                format!("{first} {second}")
            }
        }
        other => other.to_owned(),
    }
}

/// The highest `$n` a statement writes. Used only to pad a
/// `ParameterDescription` that a `Parse` left short.
fn count_parameters(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut highest = 0usize;
    let mut at = 0usize;
    let mut in_string = false;
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => in_string = !in_string,
            b'$' if !in_string => {
                let mut end = at + 1;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end > at + 1 {
                    if let Ok(n) = sql[at + 1..end].parse::<usize>() {
                        highest = highest.max(n);
                    }
                }
                at = end.saturating_sub(1);
            }
            _ => {}
        }
        at += 1;
    }
    highest
}

/// A zero value of the type an OID names, for the describe-time probe.
fn probe_param(type_oid: i32) -> Param {
    match type_oid {
        oid::BOOL => Param::Bool(false),
        oid::INT2 | oid::INT4 | oid::INT8 | oid::TIMESTAMPTZ | oid::TIMESTAMP | oid::DATE => {
            Param::Int(0)
        }
        oid::FLOAT4 | oid::FLOAT8 | oid::NUMERIC => Param::Float(0.0),
        oid::JSON | oid::JSONB => Param::Json(serde_json::Value::Null),
        oid::VECTOR => Param::Vector(Vec::new()),
        _ => Param::Text(String::new()),
    }
}

/// PostgreSQL's `statement_timeout` value, in microseconds. `None` when the
/// text is not a number with an accepted unit.
fn parse_timeout_micros(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let digits: String = text
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if digits.is_empty() {
        return None;
    }
    let amount: f64 = digits.parse().ok()?;
    let unit = text[digits.len()..].trim().to_ascii_lowercase();
    let micros_per = match unit.as_str() {
        // A bare number is MILLISECONDS, which is what PostgreSQL's own GUC
        // means when no unit is written.
        "" | "ms" => 1_000.0,
        "us" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000_000.0,
        "min" | "mins" | "minute" | "minutes" => 60_000_000.0,
        "h" | "hour" | "hours" => 3_600_000_000.0,
        "d" | "day" | "days" => 86_400_000_000.0,
        _ => return None,
    };
    Some((amount * micros_per) as u64)
}

/// The payload one committed batch becomes (`OPS_CONTRACT` §9.3): the
/// collections and edge types that moved, the key count, and whether the key
/// list was truncated. Bounded by construction -- the same L1 bound §5 makes
/// and the same 8,000-byte cap the protocol makes.
fn notification_payload(event: &ChangeEvent) -> String {
    let collections: Vec<String> = event.collections.iter().map(|c| c.0.to_string()).collect();
    let edge_types: Vec<String> = event.edge_types.iter().map(|t| format!("{t:?}")).collect();
    let mut payload = format!(
        "sequence={} collections={} edge_types={} keys={}",
        event.sequence,
        collections.join(","),
        edge_types.join(","),
        event.keys_total
    );
    if event.keys_truncated {
        payload.push_str(" keys_truncated=true");
    }
    if event.rows_affected > 0 {
        payload.push_str(&format!(" rows_affected={}", event.rows_affected));
    }
    payload.truncate(8_000);
    payload
}

/// Split a simple-query string on top-level `;`, respecting single-quoted
/// literals (a doubled `''` stays inside the string).
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            current.push(c);
            if c == '\'' {
                match chars.peek() {
                    Some('\'') => current.push(chars.next().expect("peeked")),
                    _ => in_string = false,
                }
            }
            continue;
        }
        match c {
            '\'' => {
                in_string = true;
                current.push(c);
            }
            ';' => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_owned());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out
}

fn default_gucs() -> HashMap<String, String> {
    let mut gucs = HashMap::new();
    for (key, value) in [
        ("server_version", SERVER_VERSION),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("datestyle", "ISO, MDY"),
        ("intervalstyle", "postgres"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
        ("timezone", "UTC"),
        ("search_path", "\"$user\", public"),
        ("transaction_isolation", "read committed"),
        ("default_transaction_isolation", "read committed"),
        ("transaction_read_only", "off"),
        ("extra_float_digits", "3"),
        ("application_name", ""),
        ("max_identifier_length", "63"),
        ("is_superuser", "on"),
    ] {
        gucs.insert(key.to_owned(), value.to_owned());
    }
    gucs
}
