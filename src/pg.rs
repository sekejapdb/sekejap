//! # Speaking PostgreSQL's language — the wire protocol, sans-IO
//!
//! A *wire protocol* is the exact byte-level format two programs use to talk over
//! a connection. Postgres has one, and every Postgres client (`psql`, DBeaver,
//! pgjdbc, …) speaks it. Implement that same format and those clients can connect
//! to sekejap thinking it's a real Postgres server.
//!
//! *Sans-IO* ("without I/O") is the design style: this module contains the whole
//! protocol brain but touches **no** sockets. You feed it the bytes that arrived
//! and it hands back the bytes to send; the caller owns the actual socket,
//! threads, and networking. That keeps the tricky protocol logic pure and easy to
//! test, and lets it run over any transport (see `skcli/src/pg.rs` for the thin
//! adapter that adds the real TCP socket).
//!
//! Lets any Postgres client (`psql`, DBeaver, pgjdbc, …) talk to an embedded
//! sekejap DB. This module is the whole protocol *engine* with **no I/O**: you
//! feed it received bytes and it hands back bytes to send. Sockets, threads, and
//! connection lifecycle belong to the caller (the CLI's accept loop, or a
//! downstream app in any language). The brand is still the embedded graph-first
//! multimodel database — this is a convenient access surface, not a DB server.
//!
//! Drive it like a stream transform:
//! ```ignore
//! let mut conn = pg::Connection::new(db, read_only);
//! // for each chunk read from the socket:
//! let reply = conn.feed(&chunk);   // parses complete frames, buffers partials
//! socket.write_all(&reply);
//! if conn.is_closed() { /* client sent Terminate / fatal */ }
//! ```
//!
//! Supports both the **Simple** (`Q`) and **Extended** (`Parse`/`Bind`/`Describe`
//! /`Execute`/`Sync`, incl. `$1` params) query protocols, a `pg_catalog` /
//! PostGIS introspection shim (so GUI schema trees populate), and geometry as
//! PostGIS EWKB. Trust auth — authentication + transport security are the
//! caller's responsibility.

use crate::{CoreDB, Hit};
use serde_json::{Number, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

// Startup request codes (sent in place of the protocol version).
const SSL_REQUEST: i32 = 80877103;
const GSSAPI_REQUEST: i32 = 80877104;
const CANCEL_REQUEST: i32 = 80877102;
const PROTOCOL_V3: i32 = 196608; // 0x00030000

// Sanity caps so a non-PG or hostile client can't drive a huge allocation.
const MAX_STARTUP_LEN: usize = 1 << 20; // 1 MiB
const MAX_MSG_LEN: usize = 1 << 28; // 256 MiB

// A few PG type OIDs (pg_type). We only ever send the text format, so these are
// display hints; `text` is always a safe fallback.
const OID_BOOL: i32 = 16;
const OID_INT8: i32 = 20;
const OID_FLOAT8: i32 = 701;
const OID_TEXT: i32 = 25;
const OID_JSON: i32 = 114;
/// Synthetic OID for the PostGIS `geometry` type. PostGIS assigns this dynamically
/// per-install; clients detect geometry by the type *name*, so any stable OID works.
const OID_GEOMETRY: i32 = 18000;
const GEOM_SRID: u32 = 4326; // WGS84 (lon/lat) — sekejap's GeoJSON convention

/// Is this value a GeoJSON geometry (so it should ride the wire as PostGIS EWKB)?
fn is_geojson_geometry(v: &Value) -> bool {
    if let Value::Object(m) = v {
        if let Some(Value::String(t)) = m.get("type") {
            let geom = matches!(t.as_str(),
                "Point" | "LineString" | "Polygon" | "MultiPoint"
                | "MultiLineString" | "MultiPolygon" | "GeometryCollection");
            return geom && (m.contains_key("coordinates") || m.contains_key("geometries"));
        }
    }
    false
}

/// The materialized outcome of running one statement — shared by both protocols.
enum Outcome {
    Rows {
        columns: Vec<String>,
        oids: Vec<i32>,
        rows: Vec<Vec<Option<Vec<u8>>>>,
    },
    Command(String), // command tag, e.g. "INSERT 0 3"
    Empty,
}

type SqlFail = (&'static str, String); // (SQLSTATE, message)

/// A bound portal (Extended protocol): a prepared SQL string + its bound params,
/// plus a lazily-cached execution result so Describe+Execute run the query once.
struct Portal {
    sql: String,
    params: Vec<Value>,
    cached: Option<Outcome>,
}

// ── Connection: the sans-IO state machine ────────────────────────────────────

/// One client connection's protocol state. Holds the shared DB handle, the
/// inbound byte buffer (for partial-frame reassembly), and the Extended-protocol
/// prepared-statement / portal tables. Not tied to any socket or runtime.
pub struct Connection {
    db: Arc<RwLock<CoreDB>>,
    read_only: bool,
    /// Have we finished the startup handshake (AuthenticationOk sent)?
    started: bool,
    /// Bytes received but not yet forming a complete frame.
    inbuf: Vec<u8>,
    statements: HashMap<String, String>, // prepared name → SQL
    portals: HashMap<String, Portal>,
    /// After an error inside an extended batch, skip messages until the next Sync.
    skip_until_sync: bool,
    closed: bool,
}

impl Connection {
    /// Start a new connection over a shared DB. `read_only` rejects writes.
    pub fn new(db: Arc<RwLock<CoreDB>>, read_only: bool) -> Self {
        Connection {
            db,
            read_only,
            started: false,
            inbuf: Vec::new(),
            statements: HashMap::new(),
            portals: HashMap::new(),
            skip_until_sync: false,
            closed: false,
        }
    }

    /// The client sent Terminate, a cancel request, or a fatal framing error —
    /// the caller should stop reading and drop the socket.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Feed freshly-received bytes; returns the bytes to write back. Buffers
    /// partial frames internally, so any chunking is fine. Never panics on
    /// malformed input — it replies with a Postgres ErrorResponse and/or closes.
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        self.inbuf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut pos = 0;

        while !self.closed {
            let buf = &self.inbuf[pos..];

            if !self.started {
                // Startup frame: [Int32 length][Int32 code][rest]; length is self-inclusive.
                if buf.len() < 4 {
                    break;
                }
                let len = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let len = len as usize;
                if len < 8 || len > MAX_STARTUP_LEN {
                    error_response(&mut out, "08P01", "invalid startup packet");
                    self.closed = true;
                    break;
                }
                if buf.len() < len {
                    break; // wait for the rest
                }
                let frame = buf[..len].to_vec();
                pos += len;
                self.handle_startup(&frame, &mut out);
            } else {
                // Message frame: [Int8 type][Int32 length][body of length-4].
                if buf.len() < 5 {
                    break;
                }
                let len = i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
                let len = len as usize;
                if len < 4 || len > MAX_MSG_LEN {
                    error_response(&mut out, "08P01", "invalid message length");
                    self.closed = true;
                    break;
                }
                let total = 1 + len;
                if buf.len() < total {
                    break; // wait for the rest
                }
                let typ = buf[0];
                let body = buf[5..total].to_vec();
                pos += total;
                self.handle_message(typ, &body, &mut out);
            }
        }

        if pos > 0 {
            self.inbuf.drain(..pos);
        }
        out
    }

    /// Handle a complete startup frame (`frame.len()` == its declared length,
    /// guaranteed ≥ 8 by the caller).
    fn handle_startup(&mut self, frame: &[u8], out: &mut Vec<u8>) {
        let code = i32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
        match code {
            SSL_REQUEST | GSSAPI_REQUEST => out.push(b'N'), // decline; continue plaintext
            CANCEL_REQUEST => self.closed = true,
            PROTOCOL_V3 => {
                // frame[8..] holds startup params (user/database) — unused.
                emit_startup_ok(out);
                self.started = true;
            }
            other => {
                error_response(out, "08P01", &format!("unsupported startup code {other}"));
                self.closed = true;
            }
        }
    }

    /// Handle one complete protocol message (post-startup).
    fn handle_message(&mut self, typ: u8, body: &[u8], out: &mut Vec<u8>) {
        // In error-skip mode, ignore everything except Sync/Terminate.
        if self.skip_until_sync && !matches!(typ, b'S' | b'X') {
            return;
        }

        match typ {
            // ── Simple Query ──
            b'Q' => {
                let sql = cstr_from(body);
                handle_simple_query(&self.db, self.read_only, &sql, out);
                ready_for_query(out);
            }

            // ── Extended: Parse ──
            b'P' => {
                let mut r = Reader::new(body);
                let name = r.cstr();
                let query = r.cstr();
                // (param type OIDs follow; we infer instead, so they're ignored)
                self.statements.insert(name, query);
                msg(out, b'1', &[]); // ParseComplete
            }

            // ── Extended: Bind ──
            b'B' => {
                let mut r = Reader::new(body);
                let portal = r.cstr();
                let stmt_name = r.cstr();
                let sql = match self.statements.get(&stmt_name) {
                    Some(q) => q.clone(),
                    None => {
                        error_response(out, "26000", &format!("prepared statement \"{stmt_name}\" does not exist"));
                        self.skip_until_sync = true;
                        return;
                    }
                };
                // Parameter format codes.
                //
                // These counts arrive as Int16 from the network and were cast
                // straight to `usize`. A negative one — `0xFFFF`, two bytes any
                // client can send — sign-extends to `usize::MAX`, and both uses
                // of it were fatal: `(0..usize::MAX).map(|_| r.i16())` never
                // finishes, because reads past the end of the frame return 0
                // rather than stopping, so it grows a `Vec` until the process is
                // killed; and `Vec::with_capacity(usize::MAX)` panics outright
                // with "capacity overflow", taking the connection thread with it.
                // Both are reachable by anyone who can open a socket, after a
                // `Parse` naming any statement.
                //
                // A count is also rejected when the frame has no room for what it
                // claims: a format code needs two bytes and a parameter needs at
                // least four for its own length prefix, so a message declaring
                // more than the remaining bytes can hold is malformed whatever
                // its sign.
                let nfmt = r.i16();
                if nfmt < 0 || (nfmt as usize).saturating_mul(2) > r.remaining() {
                    error_response(out, "08P01", "invalid parameter format code count");
                    self.skip_until_sync = true;
                    return;
                }
                let nfmt = nfmt as usize;
                let formats: Vec<i16> = (0..nfmt).map(|_| r.i16()).collect();
                let nparams = r.i16();
                if nparams < 0 || (nparams as usize).saturating_mul(4) > r.remaining() {
                    error_response(out, "08P01", "invalid parameter count");
                    self.skip_until_sync = true;
                    return;
                }
                let nparams = nparams as usize;
                let mut params: Vec<Value> = Vec::with_capacity(nparams);
                for i in 0..nparams {
                    let plen = r.i32();
                    if plen < 0 {
                        params.push(Value::Null);
                        continue;
                    }
                    let bytes = r.bytes(plen as usize);
                    let is_binary = match formats.len() {
                        0 => false,           // all text
                        1 => formats[0] == 1, // one code for all
                        _ => formats.get(i).copied().unwrap_or(0) == 1,
                    };
                    params.push(decode_param(bytes, is_binary));
                }
                // (result format codes ignored — we always reply in text)
                self.portals.insert(portal, Portal { sql, params, cached: None });
                msg(out, b'2', &[]); // BindComplete
            }

            // ── Extended: Describe ──
            b'D' => {
                let mut r = Reader::new(body);
                let kind = r.byte();
                let name = r.cstr();
                match kind {
                    b'S' => {
                        // Statement: ParameterDescription (0 params advertised) + NoData.
                        // We infer params, so we don't pre-declare types here.
                        if self.statements.contains_key(&name) {
                            let mut pd = Vec::new();
                            pd.extend_from_slice(&0i16.to_be_bytes());
                            msg(out, b't', &pd); // ParameterDescription
                            msg(out, b'n', &[]); // NoData
                        } else {
                            error_response(out, "26000", &format!("prepared statement \"{name}\" does not exist"));
                            self.skip_until_sync = true;
                        }
                    }
                    _ => {
                        // Portal: for a read, execute now so we can describe columns.
                        match ensure_portal_described(&self.db, self.read_only, &mut self.portals, &name) {
                            Ok(Some((columns, oids))) => emit_row_description(out, &columns, &oids),
                            Ok(None) => msg(out, b'n', &[]), // NoData (write/empty/unknown)
                            Err((state, m)) => {
                                error_response(out, state, &m);
                                self.skip_until_sync = true;
                            }
                        }
                    }
                }
            }

            // ── Extended: Execute ──
            b'E' => {
                let mut r = Reader::new(body);
                let name = r.cstr();
                let _max_rows = r.i32();
                if let Err((state, m)) = run_portal(&self.db, self.read_only, &mut self.portals, &name) {
                    error_response(out, state, &m);
                    self.skip_until_sync = true;
                    return;
                }
                // Emit from the (now cached) outcome.
                if let Some(p) = self.portals.get(&name) {
                    match &p.cached {
                        Some(Outcome::Rows { rows, .. }) => {
                            emit_data_rows(out, rows);
                            command_complete(out, &format!("SELECT {}", rows.len()));
                        }
                        Some(Outcome::Command(tag)) => command_complete(out, tag),
                        Some(Outcome::Empty) | None => empty_query_response(out),
                    }
                }
            }

            // ── Extended: Close ──
            b'C' => {
                let mut r = Reader::new(body);
                let kind = r.byte();
                let name = r.cstr();
                if kind == b'S' { self.statements.remove(&name); } else { self.portals.remove(&name); }
                msg(out, b'3', &[]); // CloseComplete
            }

            // ── Extended: Sync ──
            b'S' => {
                self.skip_until_sync = false;
                self.portals.clear(); // portals live until Sync in our simple model
                ready_for_query(out);
            }

            // ── Extended: Flush ── (no-op: `feed` already returns buffered output)
            b'H' => {}

            b'X' => self.closed = true, // Terminate

            other => {
                error_response(out, "08P01", &format!("unsupported message type '{}'", other as char));
                self.skip_until_sync = true;
            }
        }
    }
}

/// The post-auth server banner: AuthenticationOk, server parameters,
/// BackendKeyData, and the first ReadyForQuery.
fn emit_startup_ok(out: &mut Vec<u8>) {
    msg(out, b'R', &0i32.to_be_bytes()); // AuthenticationOk
    param_status(out, "server_version", &format!("16.0 (sekejap {})", env!("CARGO_PKG_VERSION")));
    param_status(out, "server_encoding", "UTF8");
    param_status(out, "client_encoding", "UTF8");
    param_status(out, "DateStyle", "ISO, MDY");
    param_status(out, "standard_conforming_strings", "on");
    param_status(out, "integer_datetimes", "on");
    param_status(out, "TimeZone", "UTC");
    let mut kd = Vec::new();
    kd.extend_from_slice(&1234i32.to_be_bytes());
    kd.extend_from_slice(&5678i32.to_be_bytes());
    msg(out, b'K', &kd);
    ready_for_query(out);
}

/// Ensure a read portal is executed + cached so we can describe its columns.
/// Returns `Some((columns, oids))` for a row-returning statement, `None` for a
/// write/empty statement (defer execution to Execute).
fn ensure_portal_described(
    db: &Arc<RwLock<CoreDB>>,
    read_only: bool,
    portals: &mut HashMap<String, Portal>,
    name: &str,
) -> Result<Option<(Vec<String>, Vec<i32>)>, SqlFail> {
    let p = match portals.get_mut(name) {
        Some(p) => p,
        None => return Err(("34000", format!("portal \"{name}\" does not exist"))),
    };
    let first = first_word(&p.sql);
    if !is_read(&first) {
        return Ok(None); // don't run writes at Describe time
    }
    if p.cached.is_none() {
        p.cached = Some(run_statement(db, read_only, &p.sql, &p.params)?);
    }
    match &p.cached {
        Some(Outcome::Rows { columns, oids, .. }) => Ok(Some((columns.clone(), oids.clone()))),
        _ => Ok(None),
    }
}

/// Ensure a portal has been executed (running it now if not already cached).
fn run_portal(
    db: &Arc<RwLock<CoreDB>>,
    read_only: bool,
    portals: &mut HashMap<String, Portal>,
    name: &str,
) -> Result<(), SqlFail> {
    let p = match portals.get_mut(name) {
        Some(p) => p,
        None => return Err(("34000", format!("portal \"{name}\" does not exist"))),
    };
    if p.cached.is_none() {
        p.cached = Some(run_statement(db, read_only, &p.sql, &p.params)?);
    }
    Ok(())
}

/// Run one Simple Query string (possibly several `;`-separated statements),
/// appending all response messages. On the first error, stops (PG abort-on-error).
fn handle_simple_query(
    db: &Arc<RwLock<CoreDB>>,
    read_only: bool,
    sql: &str,
    out: &mut Vec<u8>,
) {
    let statements = split_statements(sql);
    if statements.is_empty() {
        empty_query_response(out);
        return;
    }
    for stmt in statements {
        match run_statement(db, read_only, &stmt, &[]) {
            Ok(outcome) => emit_outcome(out, outcome),
            Err((state, m)) => {
                error_response(out, state, &m);
                return;
            }
        }
    }
}

/// Execute a single statement with optional bound params, materializing the
/// result. Shared by both protocols.
fn run_statement(
    db: &Arc<RwLock<CoreDB>>,
    read_only: bool,
    stmt: &str,
    params: &[Value],
) -> Result<Outcome, SqlFail> {
    let trimmed = stmt.trim();
    let first = first_word(trimmed);

    if first.is_empty() {
        return Ok(Outcome::Empty);
    }
    if std::env::var("SEKEJAP_PG_DEBUG").is_ok() {
        eprintln!("[pg] {trimmed}");
    }
    // Postgres session/catalog compatibility — answer the connect-time GUC/version/
    // catalog chatter that clients (DBeaver, pgjdbc) send, without bothering the
    // engine. Real sekejap SQL falls through (returns None).
    if let Some(o) = pg_shim(db, &first, trimmed, params) {
        return Ok(o);
    }
    if is_read(&first) {
        let guard = db.read().map_err(lock_err)?;
        if first == "SHOW" {
            let hits = guard.show(trimmed).map_err(query_err)?;
            return Ok(build_rows(&hits));
        }
        let set = if params.is_empty() {
            guard.query(trimmed).map_err(query_err)?
        } else {
            guard.query_params(trimmed, params).map_err(query_err)?
        };
        let hits: Vec<Hit> = set.collect();
        Ok(build_rows(&hits))
    } else {
        if read_only {
            return Err(("25006", "server is read-only".to_string()));
        }
        let mut guard = db.write().map_err(lock_err)?;
        let n = if params.is_empty() {
            guard.execute(trimmed).map_err(query_err)?
        } else {
            guard.execute_params(trimmed, params).map_err(query_err)?
        };
        Ok(Outcome::Command(command_tag(&first, trimmed, n)))
    }
}

fn emit_outcome(out: &mut Vec<u8>, outcome: Outcome) {
    match outcome {
        Outcome::Rows { columns, oids, rows } => {
            emit_row_description(out, &columns, &oids);
            let n = rows.len();
            emit_data_rows(out, &rows);
            command_complete(out, &format!("SELECT {n}"));
        }
        Outcome::Command(tag) => command_complete(out, &tag),
        Outcome::Empty => empty_query_response(out),
    }
}

// ── Result materialization ───────────────────────────────────────────────────

/// Build a wire-ready result set from hits. Columns are the ordered union of
/// payload object keys (mirrors the CLI table renderer); with no structured
/// payload, a single `_slug` column is used.
fn build_rows(hits: &[Hit]) -> Outcome {
    let mut columns: Vec<String> = Vec::new();
    for h in hits {
        if let Some(Value::Object(map)) = &h.payload {
            for k in map.keys() {
                if !columns.contains(k) {
                    columns.push(k.clone());
                }
            }
        }
    }
    let slug_mode = columns.is_empty();
    if slug_mode {
        columns.push("_slug".to_string());
    }
    let oids: Vec<i32> = columns.iter()
        .map(|c| if slug_mode { OID_TEXT } else { infer_oid(hits, c) })
        .collect();

    let mut rows: Vec<Vec<Option<Vec<u8>>>> = Vec::with_capacity(hits.len());
    for h in hits {
        let mut row = Vec::with_capacity(columns.len());
        if slug_mode {
            row.push(Some(h.slug.clone().into_bytes()));
        } else {
            let obj = match &h.payload { Some(Value::Object(m)) => Some(m), _ => None };
            for col in &columns {
                row.push(obj.and_then(|m| m.get(col)).and_then(value_text));
            }
        }
        rows.push(row);
    }
    Outcome::Rows { columns, oids, rows }
}

/// Pick a display OID for a column by scanning its values. Homogeneous columns
/// get a precise type; anything mixed falls back to `text`.
fn infer_oid(hits: &[Hit], col: &str) -> i32 {
    // Uniform GeoJSON column → PostGIS geometry.
    let (mut saw_geo, mut all_geo) = (false, true);
    for h in hits {
        if let Some(Value::Object(m)) = &h.payload {
            match m.get(col) {
                None | Some(Value::Null) => {}
                Some(v) if is_geojson_geometry(v) => saw_geo = true,
                Some(_) => all_geo = false,
            }
        }
    }
    if saw_geo && all_geo { return OID_GEOMETRY; }

    let (mut ints, mut floats, mut bools, mut strings, mut jsons, mut any) =
        (false, false, false, false, false, false);
    for h in hits {
        if let Some(Value::Object(m)) = &h.payload {
            match m.get(col) {
                None | Some(Value::Null) => {}
                Some(Value::Bool(_)) => { bools = true; any = true; }
                Some(Value::Number(n)) => {
                    any = true;
                    if n.is_i64() || n.is_u64() { ints = true; } else { floats = true; }
                }
                Some(Value::String(_)) => { strings = true; any = true; }
                Some(Value::Array(_)) | Some(Value::Object(_)) => { jsons = true; any = true; }
            }
        }
    }
    if !any { return OID_TEXT; }
    match (ints, floats, bools, strings, jsons) {
        (true, false, false, false, false) => OID_INT8,
        (_, true, false, false, false) if !strings && !bools && !jsons => OID_FLOAT8,
        (false, false, true, false, false) => OID_BOOL,
        (false, false, false, false, true) => OID_JSON,
        _ => OID_TEXT,
    }
}

/// Render a JSON value in PG text format. `None` means SQL NULL. GeoJSON
/// geometries become PostGIS **EWKB hex** so spatial clients render them on a map.
fn value_text(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone().into_bytes()),
        Value::Bool(b) => Some(if *b { b"t".to_vec() } else { b"f".to_vec() }),
        Value::Number(n) => Some(n.to_string().into_bytes()),
        _ if is_geojson_geometry(v) => {
            crate::geo::geojson_to_ewkb_hex(v, GEOM_SRID).map(|s| s.into_bytes())
        }
        other => Some(other.to_string().into_bytes()), // arrays/objects → JSON text
    }
}

/// Decode a bound parameter into a JSON value. Text params are parsed as
/// int/float when they look numeric (so `WHERE age > $1` works), else kept as a
/// string. Binary params get a best-effort integer decode by width.
fn decode_param(bytes: &[u8], is_binary: bool) -> Value {
    if is_binary {
        match bytes.len() {
            2 => Value::Number(Number::from(i16::from_be_bytes([bytes[0], bytes[1]]) as i64)),
            4 => Value::Number(Number::from(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64)),
            8 => Value::Number(Number::from(i64::from_be_bytes(bytes.try_into().unwrap()))),
            _ => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        }
    } else {
        let s = String::from_utf8_lossy(bytes);
        if let Ok(i) = s.parse::<i64>() {
            Value::Number(Number::from(i))
        } else if let Ok(f) = s.parse::<f64>() {
            Number::from_f64(f).map(Value::Number).unwrap_or(Value::String(s.into_owned()))
        } else {
            Value::String(s.into_owned())
        }
    }
}

/// PG command tag. `INSERT` uses the `INSERT 0 <n>` form; DDL keeps its first two
/// words (e.g. `CREATE TABLE`); others are `<VERB> <n>`.
fn command_tag(first: &str, stmt: &str, n: usize) -> String {
    match first {
        "INSERT" => format!("INSERT 0 {n}"),
        "UPDATE" => format!("UPDATE {n}"),
        "DELETE" => format!("DELETE {n}"),
        "CREATE" | "DROP" | "ALTER" => {
            let mut w = stmt.split_whitespace();
            let a = w.next().unwrap_or("").to_ascii_uppercase();
            match w.next() {
                Some(b) => format!("{a} {}", b.to_ascii_uppercase()),
                None => a,
            }
        }
        other => format!("{other} {n}"),
    }
}

fn first_word(s: &str) -> String {
    s.trim().split_whitespace().next().unwrap_or("").to_ascii_uppercase()
}

fn second_word(s: &str) -> String {
    s.trim().split_whitespace().nth(1).unwrap_or("").to_ascii_uppercase()
}

/// A single-row, single-column text result — the shape of most GUC/function
/// replies a Postgres client expects.
fn one_cell(col: &str, val: &str) -> Outcome {
    Outcome::Rows {
        columns: vec![col.to_string()],
        oids: vec![OID_TEXT],
        rows: vec![vec![Some(val.as_bytes().to_vec())]],
    }
}

fn pg_version() -> String {
    format!("PostgreSQL 16.0 (sekejap {}) on {}, 64-bit", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH)
}

/// Canned value for a Postgres GUC (used by `SHOW <var>` and `current_setting`).
fn guc_value(var: &str) -> &'static str {
    match var {
        "SERVER_VERSION" => "16.0",
        "SERVER_ENCODING" | "CLIENT_ENCODING" => "UTF8",
        "DATESTYLE" => "ISO, MDY",
        "STANDARD_CONFORMING_STRINGS" | "INTEGER_DATETIMES" => "on",
        "TRANSACTION_ISOLATION" | "DEFAULT_TRANSACTION_ISOLATION" => "read committed",
        "TRANSACTION_READ_ONLY" => "off",
        "TIMEZONE" | "TIME ZONE" => "UTC",
        "SEARCH_PATH" => "\"$user\", public",
        "EXTRA_FLOAT_DIGITS" => "3",
        "APPLICATION_NAME" => "",
        "IS_SUPERUSER" => "on",
        "MAX_IDENTIFIER_LENGTH" => "63",
        _ => "",
    }
}

/// Top-level Postgres compatibility shim. Handles the connect-time chatter that
/// clients (DBeaver, pgjdbc) send: session GUCs, `version()`, and — crucially for
/// a populated schema tree — `pg_catalog` introspection synthesized from the live
/// collections. Returns `None` for real sekejap SQL (→ engine).
fn pg_shim(
    db: &Arc<RwLock<CoreDB>>,
    first: &str,
    sql: &str,
    params: &[Value],
) -> Option<Outcome> {
    let upper = sql.to_ascii_uppercase();
    let is_catalog = [
        "PG_CATALOG", "INFORMATION_SCHEMA", "PG_TYPE", "PG_NAMESPACE", "PG_CLASS",
        "PG_ATTRIBUTE", "PG_DATABASE", "PG_ENUM", "PG_ROLES", "PG_SETTINGS",
        "PG_PROC", "PG_GET_KEYWORDS", "PG_INDEX", "PG_CONSTRAINT",
        "GEOMETRY_COLUMNS", "SPATIAL_REF_SYS", "PG_EXTENSION",
    ].iter().any(|k| upper.contains(k));
    if is_catalog {
        return Some(catalog_response(db, &upper, params));
    }
    pg_session_shim(first, sql)
}

/// Synthesize a `pg_catalog` reply from the live DB. We return only the columns
/// DBeaver actually reads (it uses null-tolerant getters), keyed by the catalog
/// table referenced. Single hard-coded `public` schema for now.
fn catalog_response(db: &Arc<RwLock<CoreDB>>, upper: &str, params: &[Value]) -> Outcome {
    // Type/relation probes like `... WHERE 1<>1 LIMIT 1` want an empty set.
    if upper.contains("1<>1") || upper.contains("1 <> 1") {
        return empty_set();
    }
    // pgjdbc type resolution: `... WHERE t.oid = $1`. Fired for every OID a result
    // set uses that the driver doesn't know (our geometry OID 18000). Resolving it
    // to ("public","geometry") is what makes DBeaver treat the column as PostGIS
    // geometry. Two shapes, both keyed on `current_schemas`.
    if upper.contains("CURRENT_SCHEMAS") && upper.contains("PG_TYPE") {
        let oid = params.first().and_then(|v| v.as_i64())
            .or_else(|| extract_int_after(upper, "OID ="))
            .or_else(|| extract_int_after(upper, "OID="))
            .unwrap_or(0);
        let info = type_info(oid);
        if upper.contains("ARRAY_IN") || upper.contains("IS_ARRAY") {
            // is_array, typtype, typname, oid
            let cols = [("is_array", OID_BOOL), ("typtype", OID_TEXT),
                        ("typname", OID_TEXT), ("oid", OID_INT8)];
            return match info {
                Some((_ns, tn, tt)) => build(&cols, vec![vec![boolcell(false), s(tt), s(tn), i(oid)]]),
                None => empty_set(),
            };
        }
        // <schema on search_path?> (bool), nspname, typname
        let cols = [("in_search_path", OID_BOOL), ("nspname", OID_TEXT), ("typname", OID_TEXT)];
        return match info {
            Some((ns, tn, _tt)) => build(&cols, vec![vec![boolcell(true), s(ns), s(tn)]]),
            None => empty_set(),
        };
    }
    if upper.contains("GEOMETRY_COLUMNS") {
        return catalog_geometry_columns(db);
    }
    if upper.contains("SPATIAL_REF_SYS") {
        return catalog_spatial_ref_sys();
    }
    if upper.contains("PG_NAMESPACE") {
        return catalog_namespaces();
    }
    if upper.contains("PG_DATABASE") {
        return catalog_database(params);
    }
    if upper.contains("PG_ATTRIBUTE") {
        return catalog_columns(db, upper);
    }
    if upper.contains("PG_TYPE") {
        return catalog_types();
    }
    if upper.contains("PG_CLASS")
        && (upper.contains("RELKIND") || upper.contains("RELNAMESPACE") || upper.contains("RELNAME"))
    {
        return catalog_tables(db);
    }
    // pg_enum / pg_settings / pg_roles / pg_get_keywords / information_schema / …
    empty_set()
}

const NS_OID: i64 = 2200; // "public"

/// (schema, type name, typtype) for an OID — mirrors `catalog_types`. Used to
/// answer pgjdbc's on-demand type-resolution query. `geometry` lives in `public`.
fn type_info(oid: i64) -> Option<(&'static str, &'static str, &'static str)> {
    let (ns, name) = match oid {
        16 => ("pg_catalog", "bool"), 20 => ("pg_catalog", "int8"),
        21 => ("pg_catalog", "int2"), 23 => ("pg_catalog", "int4"),
        25 => ("pg_catalog", "text"), 700 => ("pg_catalog", "float4"),
        701 => ("pg_catalog", "float8"), 114 => ("pg_catalog", "json"),
        1043 => ("pg_catalog", "varchar"), 1700 => ("pg_catalog", "numeric"),
        1184 => ("pg_catalog", "timestamptz"),
        o if o == OID_GEOMETRY as i64 => ("public", "geometry"),
        _ => return None,
    };
    Some((ns, name, "b"))
}

/// Stable synthetic OID for a collection (kept in the user-object range).
fn table_oid(name: &str) -> i64 {
    // FNV-1a over the name → offset into a safe range above the system OIDs.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    16_385 + (h % 2_000_000_000) as i64
}

fn list_collections(guard: &CoreDB) -> Vec<String> {
    match guard.show("SHOW TABLES") {
        Ok(hits) => hits.iter().filter_map(|h| {
            h.payload.as_ref()
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }).collect(),
        Err(_) => Vec::new(),
    }
}

/// (column name, PG type OID, PG type name, not-null, is-primary-key) for a
/// collection — from its declared schema, or sampled from a row when schemaless.
fn collection_columns(guard: &CoreDB, name: &str) -> Vec<(String, i64, String, bool, bool)> {
    use crate::sql::FieldType;
    if let Some(schema) = guard.table_schema(name) {
        return schema.fields.iter().map(|f| {
            let (oid, tn): (i64, &str) = match f.ty {
                FieldType::Text        => (25, "text"),
                FieldType::Integer     => (20, "int8"),
                FieldType::Real        => (701, "float8"),
                FieldType::Bool        => (16, "bool"),
                FieldType::Timestamptz => (1184, "timestamptz"),
                FieldType::Geo         => (OID_GEOMETRY as i64, "geometry"),
                FieldType::Vector      => (25, "text"),
                FieldType::Json        => (114, "json"),
            };
            (f.name.clone(), oid, tn.to_string(), f.is_primary_key, f.is_primary_key)
        }).collect();
    }
    // Schemaless — sample one row and infer per-field types.
    if let Ok(set) = guard.query(&format!("SELECT * FROM {name} LIMIT 1")) {
        if let Some(h) = set.collect().first() {
            if let Some(Value::Object(map)) = &h.payload {
                return map.iter().map(|(k, v)| {
                    let (oid, tn): (i64, &str) = match v {
                        _ if is_geojson_geometry(v) => (OID_GEOMETRY as i64, "geometry"),
                        Value::Bool(_) => (16, "bool"),
                        Value::Number(n) if n.is_f64() => (701, "float8"),
                        Value::Number(_) => (20, "int8"),
                        Value::Array(_) | Value::Object(_) => (114, "json"),
                        _ => (25, "text"),
                    };
                    (k.clone(), oid, tn.to_string(), k == "_key", k == "_key")
                }).collect();
            }
        }
    }
    vec![("_key".to_string(), 25, "text".to_string(), true, true)]
}

// ── Catalog row builders ─────────────────────────────────────────────────────

fn s(v: &str) -> Option<Vec<u8>> { Some(v.as_bytes().to_vec()) }
fn i(v: i64) -> Option<Vec<u8>> { Some(v.to_string().into_bytes()) }
fn boolcell(v: bool) -> Option<Vec<u8>> { Some(if v { b"t".to_vec() } else { b"f".to_vec() }) }

fn build(cols: &[(&str, i32)], rows: Vec<Vec<Option<Vec<u8>>>>) -> Outcome {
    Outcome::Rows {
        columns: cols.iter().map(|(n, _)| n.to_string()).collect(),
        oids: cols.iter().map(|(_, o)| *o).collect(),
        rows,
    }
}

fn empty_set() -> Outcome {
    Outcome::Rows { columns: vec![], oids: vec![], rows: vec![] }
}

fn catalog_namespaces() -> Outcome {
    let cols = [("oid", OID_INT8), ("nspname", OID_TEXT), ("nspowner", OID_INT8),
                ("nspacl", OID_TEXT), ("description", OID_TEXT)];
    build(&cols, vec![vec![i(NS_OID), s("public"), i(10), None, None]])
}

fn catalog_database(params: &[Value]) -> Outcome {
    let name = params.first().and_then(|v| v.as_str()).unwrap_or("sekejap");
    let cols = [("oid", OID_INT8), ("datname", OID_TEXT), ("datdba", OID_INT8),
                ("encoding", OID_INT8), ("datcollate", OID_TEXT), ("datctype", OID_TEXT),
                ("datistemplate", OID_BOOL), ("datallowconn", OID_BOOL), ("description", OID_TEXT)];
    build(&cols, vec![vec![
        i(16_400), s(name), i(10), i(6), s("C"), s("C"), boolcell(false), boolcell(true), None,
    ]])
}

fn catalog_tables(db: &Arc<RwLock<CoreDB>>) -> Outcome {
    let cols = [("oid", OID_INT8), ("relname", OID_TEXT), ("relnamespace", OID_INT8),
                ("relkind", OID_TEXT), ("relpersistence", OID_TEXT), ("relowner", OID_INT8),
                ("reltuples", OID_FLOAT8), ("relhasindex", OID_BOOL), ("relnatts", OID_INT8),
                ("relhassubclass", OID_BOOL), ("relispartition", OID_BOOL),
                ("relam", OID_INT8), ("reltablespace", OID_INT8), ("description", OID_TEXT)];
    let guard = match db.read() { Ok(g) => g, Err(_) => return build(&cols, vec![]) };
    let rows = list_collections(&guard).into_iter().map(|name| {
        let ncols = collection_columns(&guard, &name).len() as i64;
        vec![
            i(table_oid(&name)), s(&name), i(NS_OID), s("r"), s("p"), i(10),
            i(0), boolcell(false), i(ncols), boolcell(false), boolcell(false),
            i(0), i(0), None,
        ]
    }).collect();
    build(&cols, rows)
}

fn catalog_columns(db: &Arc<RwLock<CoreDB>>, upper: &str) -> Outcome {
    let cols = [("attrelid", OID_INT8), ("attname", OID_TEXT), ("attnum", OID_INT8),
                ("atttypid", OID_INT8), ("atttypmod", OID_INT8), ("attnotnull", OID_BOOL),
                ("atthasdef", OID_BOOL), ("attidentity", OID_TEXT), ("attgenerated", OID_TEXT),
                ("attisdropped", OID_BOOL), ("attislocal", OID_BOOL),
                ("attribute_type", OID_TEXT), ("def_value", OID_TEXT), ("description", OID_TEXT)];
    let guard = match db.read() { Ok(g) => g, Err(_) => return build(&cols, vec![]) };
    let want = extract_int_after(upper, "ATTRELID");
    let mut rows = Vec::new();
    for name in list_collections(&guard) {
        if want.is_some() && want != Some(table_oid(&name)) {
            continue;
        }
        let reloid = table_oid(&name);
        for (n, (cname, typoid, typname, notnull, _pk)) in
            collection_columns(&guard, &name).into_iter().enumerate()
        {
            rows.push(vec![
                i(reloid), s(&cname), i(n as i64 + 1), i(typoid), i(-1),
                boolcell(notnull), boolcell(false), s(""), s(""), boolcell(false),
                boolcell(true), s(&typname), None, None,
            ]);
        }
    }
    build(&cols, rows)
}

fn catalog_types() -> Outcome {
    let cols = [("oid", OID_INT8), ("typname", OID_TEXT), ("typtype", OID_TEXT),
                ("typcategory", OID_TEXT), ("typlen", OID_INT8), ("typrelid", OID_INT8),
                ("typbasetype", OID_INT8), ("typnamespace", OID_INT8), ("nspname", OID_TEXT),
                ("base_type_name", OID_TEXT), ("relkind", OID_TEXT), ("description", OID_TEXT),
                ("typelem", OID_INT8), ("typarray", OID_INT8)];
    // (oid, name, category, len)
    let types: &[(i64, &str, &str, i64)] = &[
        (16, "bool", "B", 1), (20, "int8", "N", 8), (21, "int2", "N", 2),
        (23, "int4", "N", 4), (25, "text", "S", -1), (700, "float4", "N", 4),
        (701, "float8", "N", 8), (114, "json", "U", -1), (1043, "varchar", "S", -1),
        (1700, "numeric", "N", -1), (1184, "timestamptz", "D", 8),
    ];
    let mut rows: Vec<Vec<Option<Vec<u8>>>> = types.iter().map(|(oid, name, cat, len)| vec![
        i(*oid), s(name), s("b"), s(cat), i(*len), i(0), i(0), i(11),
        s("pg_catalog"), None, None, None, i(0), i(0),
    ]).collect();
    // PostGIS geometry — lives in `public`, category 'U' (user), so spatial
    // clients (DBeaver, QGIS) recognize geometry columns and render them.
    rows.push(vec![
        i(OID_GEOMETRY as i64), s("geometry"), s("b"), s("U"), i(-1), i(0), i(0),
        i(NS_OID), s("public"), None, None, None, i(0), i(0),
    ]);
    build(&cols, rows)
}

/// PostGIS `geometry_columns` view — one row per geometry column across all
/// collections. Spatial clients read this to discover mappable columns + SRID.
fn catalog_geometry_columns(db: &Arc<RwLock<CoreDB>>) -> Outcome {
    let cols = [("f_table_catalog", OID_TEXT), ("f_table_schema", OID_TEXT),
                ("f_table_name", OID_TEXT), ("f_geometry_column", OID_TEXT),
                ("coord_dimension", OID_INT8), ("srid", OID_INT8), ("type", OID_TEXT)];
    let guard = match db.read() { Ok(g) => g, Err(_) => return build(&cols, vec![]) };
    let mut rows = Vec::new();
    for name in list_collections(&guard) {
        for (cname, oid, _tn, _nn, _pk) in collection_columns(&guard, &name) {
            if oid == OID_GEOMETRY as i64 {
                rows.push(vec![
                    s("sekejap"), s("public"), s(&name), s(&cname),
                    i(2), i(GEOM_SRID as i64), s("GEOMETRY"),
                ]);
            }
        }
    }
    build(&cols, rows)
}

/// Minimal `spatial_ref_sys` — just WGS84 (4326), the SRID we tag geometries with.
fn catalog_spatial_ref_sys() -> Outcome {
    let cols = [("srid", OID_INT8), ("auth_name", OID_TEXT), ("auth_srid", OID_INT8),
                ("srtext", OID_TEXT), ("proj4text", OID_TEXT)];
    build(&cols, vec![vec![
        i(4326), s("EPSG"), i(4326),
        s("GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],\
           PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]"),
        s("+proj=longlat +datum=WGS84 +no_defs"),
    ]])
}

/// Parse the first integer appearing after `key` in an (upper-cased) SQL string,
/// e.g. `ATTRELID = 16385` → `Some(16385)`. Used to map a catalog query back to a
/// collection by its synthetic OID.
fn extract_int_after(upper: &str, key: &str) -> Option<i64> {
    let idx = upper.find(key)? + key.len();
    let tail = &upper[idx..];
    let start = tail.find(|c: char| c.is_ascii_digit())?;
    let digits: String = tail[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Intercept Postgres session statements (GUCs, transactions, `version()`,
/// `current_*`). Returns `None` for real sekejap SQL (→ engine).
fn pg_session_shim(first: &str, sql: &str) -> Option<Outcome> {
    let body = sql.trim().trim_end_matches(';');
    let upper = body.to_ascii_uppercase();

    match first {
        // Session variables — ack everything except sekejap's own WAL_* knobs.
        "SET" => {
            let var = second_word(body);
            if matches!(var.as_str(), "WAL_FORMAT" | "WAL_MODE" | "WAL_SYNC") {
                None // real engine setting
            } else {
                Some(Outcome::Command("SET".to_string()))
            }
        }
        "RESET" => Some(Outcome::Command("RESET".to_string())),
        "DISCARD" => Some(Outcome::Command("DISCARD ALL".to_string())),

        // Transaction control — acked as no-ops. Our per-statement model already
        // commits each write via the WAL, and engine transaction state is global
        // to the shared DB (unsafe to drive per-connection), so we don't forward.
        "BEGIN" | "START" => Some(Outcome::Command("BEGIN".to_string())),
        "COMMIT" | "END" => Some(Outcome::Command("COMMIT".to_string())),
        "ROLLBACK" | "ABORT" => Some(Outcome::Command("ROLLBACK".to_string())),

        // SHOW <guc> — canned; sekejap's own SHOW targets fall through to engine.
        "SHOW" => {
            let var = second_word(body);
            match var.as_str() {
                "TABLES" | "EDGES" | "INDEX" | "INDEXES" | "COLLECTIONS" => None,
                "ALL" => None,
                _ => Some(one_cell(&var.to_ascii_lowercase(), guc_value(&var))),
            }
        }

        // Function-only SELECTs from the connect handshake (catalog SELECTs are
        // handled earlier in pg_shim).
        "SELECT" => {
            if upper.starts_with("SELECT VERSION()") {
                return Some(one_cell("version", &pg_version()));
            }
            // e.g. `SELECT current_schema(),session_user` — two columns.
            if upper.contains("CURRENT_SCHEMA") && (upper.contains("SESSION_USER") || upper.contains("CURRENT_USER")) {
                return Some(build(
                    &[("current_schema", OID_TEXT), ("session_user", OID_TEXT)],
                    vec![vec![s("public"), s("postgres")]],
                ));
            }
            if upper.contains("CURRENT_SCHEMA") {
                return Some(one_cell("current_schema", "public"));
            }
            if upper.contains("CURRENT_DATABASE") {
                return Some(one_cell("current_database", "sekejap"));
            }
            if upper.contains("CURRENT_USER") || upper.contains("SESSION_USER") || upper.contains("CURRENT_ROLE") {
                return Some(one_cell("current_user", "postgres"));
            }
            None
        }
        _ => None,
    }
}

fn is_read(first: &str) -> bool {
    matches!(first, "SELECT" | "MATCH" | "WITH" | "TABLE" | "VALUES" | "EXPLAIN" | "SHOW")
}

// ── Low-level message writers ────────────────────────────────────────────────

/// Frame a message: 1-byte type, Int32 length (self-inclusive), body.
fn msg(out: &mut Vec<u8>, typ: u8, body: &[u8]) {
    out.push(typ);
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
}

fn emit_row_description(out: &mut Vec<u8>, columns: &[String], oids: &[i32]) {
    let mut rd = Vec::new();
    rd.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    for (col, oid) in columns.iter().zip(oids.iter()) {
        cstr(&mut rd, col);
        rd.extend_from_slice(&0i32.to_be_bytes());    // table OID
        rd.extend_from_slice(&0i16.to_be_bytes());    // column attr number
        rd.extend_from_slice(&oid.to_be_bytes());     // type OID
        rd.extend_from_slice(&(-1i16).to_be_bytes()); // type size (variable)
        rd.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        rd.extend_from_slice(&0i16.to_be_bytes());    // format code (text)
    }
    msg(out, b'T', &rd);
}

fn emit_data_rows(out: &mut Vec<u8>, rows: &[Vec<Option<Vec<u8>>>]) {
    for row in rows {
        let mut dr = Vec::new();
        dr.extend_from_slice(&(row.len() as i16).to_be_bytes());
        for cell in row {
            match cell {
                None => dr.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(b) => {
                    dr.extend_from_slice(&(b.len() as i32).to_be_bytes());
                    dr.extend_from_slice(b);
                }
            }
        }
        msg(out, b'D', &dr);
    }
}

fn param_status(out: &mut Vec<u8>, key: &str, val: &str) {
    let mut b = Vec::new();
    cstr(&mut b, key);
    cstr(&mut b, val);
    msg(out, b'S', &b);
}

fn ready_for_query(out: &mut Vec<u8>) {
    msg(out, b'Z', b"I"); // 'I' = idle (not in a transaction block)
}

fn command_complete(out: &mut Vec<u8>, tag: &str) {
    let mut b = Vec::new();
    cstr(&mut b, tag);
    msg(out, b'C', &b);
}

fn empty_query_response(out: &mut Vec<u8>) {
    msg(out, b'I', &[]);
}

fn error_response(out: &mut Vec<u8>, sqlstate: &str, message: &str) {
    let mut b = Vec::new();
    b.push(b'S'); cstr(&mut b, "ERROR");
    b.push(b'V'); cstr(&mut b, "ERROR");
    b.push(b'C'); cstr(&mut b, sqlstate);
    b.push(b'M'); cstr(&mut b, message);
    b.push(0); // field terminator
    msg(out, b'E', &b);
}

fn cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

// ── Low-level readers / parsing ──────────────────────────────────────────────

/// Cursor over a message body for the Extended protocol.
struct Reader<'a> { b: &'a [u8], pos: usize }
impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self { Reader { b, pos: 0 } }
    fn byte(&mut self) -> u8 {
        let v = self.b.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        v
    }
    fn i16(&mut self) -> i16 {
        let mut a = [0u8; 2];
        for x in &mut a { *x = self.byte(); }
        i16::from_be_bytes(a)
    }
    fn i32(&mut self) -> i32 {
        let mut a = [0u8; 4];
        for x in &mut a { *x = self.byte(); }
        i32::from_be_bytes(a)
    }
    /// Bytes still unread in this frame.
    ///
    /// A count field says how many items follow; this says how many could
    /// possibly fit. A message claiming more than it has room for is malformed,
    /// and believing it is how a two-byte field turned into an allocation of
    /// `usize::MAX`.
    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.pos)
    }
    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let end = (self.pos + n).min(self.b.len());
        let out = &self.b[self.pos..end];
        self.pos = end;
        out
    }
    fn cstr(&mut self) -> String {
        let start = self.pos;
        while self.pos < self.b.len() && self.b[self.pos] != 0 { self.pos += 1; }
        let s = String::from_utf8_lossy(&self.b[start..self.pos]).into_owned();
        if self.pos < self.b.len() { self.pos += 1; } // skip NUL
        s
    }
}

/// Read a null-terminated string from the front of a body buffer (Simple query).
fn cstr_from(body: &[u8]) -> String {
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    String::from_utf8_lossy(&body[..end]).into_owned()
}

/// Split a query string into statements on top-level `;`, respecting single-quoted
/// string literals (a doubled `''` stays inside the string). Dollar-quoting is not
/// handled — fine for a dev access port.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            cur.push(c);
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    cur.push(chars.next().unwrap());
                } else {
                    in_str = false;
                }
            }
        } else {
            match c {
                '\'' => { in_str = true; cur.push(c); }
                ';' => {
                    if !cur.trim().is_empty() { out.push(cur.trim().to_string()); }
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn lock_err<T>(_: T) -> SqlFail { ("XX000", "internal lock poisoned".to_string()) }
fn query_err<E: std::fmt::Display>(e: E) -> SqlFail { ("42601", e.to_string()) }
