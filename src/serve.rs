//! # The HTTP/JSON surface — routing requests, sans-IO
//!
//! This is the HTTP counterpart to `pg.rs`: it lets a web client query sekejap
//! over plain HTTP + JSON. Like `pg.rs` it is *sans-IO* ("without I/O") — it
//! contains only the decision logic: given a parsed request `(method, path,
//! body)` it returns a response `(status, json)`, and touches no sockets or async
//! runtime. The caller (the CLI's server in `skcli/src/serve.rs`) owns the actual
//! networking and just wires it into [`handle`]. Keeping I/O out makes the routing
//! trivial to test and reusable under any web server.
//!
//! Given a parsed request `(method, path, body)`, produce a response
//! `(status, json)`. No sockets, no async runtime, no web framework: the caller
//! owns all of that. Wire [`handle`] into any HTTP server — the CLI's axum
//! adapter, or a downstream app's stack (axum / FastAPI / express / …) via FFI.
//!
//! This is the whole HTTP surface: `/query` (SQL/graph/agg, `$1` params),
//! `/graph` (upsert nodes+edges), `DELETE /graph` (remove), `/health`, `/version`.
//! Reads take a shared lock, writes an exclusive lock; `read_only` rejects writes.
//! Authentication is the caller's job (it owns the transport + headers).

use crate::CoreDB;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};

/// A parsed HTTP request. `body` is the raw request body (expected JSON for POST).
pub struct HttpRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a [u8],
}

/// The service's response: an HTTP status and a JSON body string.
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    fn ok(v: Value) -> Self { Self { status: 200, body: v.to_string() } }
    fn json(status: u16, v: Value) -> Self { Self { status, body: v.to_string() } }
    /// `{ "ok": false, "error": { "code", "message" } }` with the given status.
    fn err(status: u16, code: &str, msg: &str) -> Self {
        Self::json(status, json!({ "ok": false, "error": { "code": code, "message": msg } }))
    }
}

#[derive(Deserialize, Default)]
struct QueryReq {
    sql: String,
    #[serde(default)]
    params: Vec<Value>,
}

/// One edge in a `/graph` request. `from`/`to` are node slugs (`"collection/key"`);
/// `attrs` is an optional JSON object of edge properties.
#[derive(Deserialize)]
struct EdgeSpec {
    from: String,
    to: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    attrs: Option<Value>,
}

#[derive(Deserialize, Default)]
struct GraphBody {
    #[serde(default)]
    nodes: Vec<Value>,
    #[serde(default)]
    edges: Vec<EdgeSpec>,
}

/// Route a request to the shared DB and return the response. Recovers a poisoned
/// lock rather than panicking, so one bad request never wedges the server.
pub fn handle(db: &Arc<RwLock<CoreDB>>, req: &HttpRequest, read_only: bool) -> HttpResponse {
    match (req.method, req.path) {
        ("GET", "/health") => HttpResponse::ok(json!({ "ok": true })),
        ("GET", "/version") => HttpResponse::ok(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })),
        ("POST", "/query") => query(db, req.body, read_only),
        ("POST", "/graph") => graph_post(db, req.body, read_only),
        ("DELETE", "/graph") => graph_delete(db, req.body, read_only),
        _ => HttpResponse::err(404, "not_found", "unknown route"),
    }
}

fn query(db: &Arc<RwLock<CoreDB>>, body: &[u8], read_only: bool) -> HttpResponse {
    let req: QueryReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::err(400, "bad_json", &e.to_string()),
    };
    let sql = req.sql.trim().to_string();
    let first = sql.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
    let is_read = matches!(first.as_str(), "SELECT" | "MATCH" | "SHOW" | "WITH");
    let start = std::time::Instant::now();

    // ── Write path (DML/DDL) — exclusive lock ──
    if !is_read {
        if read_only {
            return HttpResponse::err(403, "read_only", "server is read-only");
        }
        let mut g = db.write().unwrap_or_else(|e| e.into_inner());
        let res = if req.params.is_empty() { g.execute(&sql) } else { g.execute_params(&sql, &req.params) };
        return match res {
            Ok(n) => HttpResponse::ok(json!({ "ok": true, "affected": n, "took_ms": start.elapsed().as_millis() as u64 })),
            Err(e) => HttpResponse::err(400, "sql_error", &e.to_string()),
        };
    }

    // ── Read path — shared lock, concurrent ──
    let g = db.read().unwrap_or_else(|e| e.into_inner());
    let set = if req.params.is_empty() { g.query(&sql) } else { g.query_params(&sql, &req.params) };
    let hits = match set {
        Ok(s) => s.collect(),
        Err(e) => return HttpResponse::err(400, "sql_error", &e.to_string()),
    };
    drop(g);
    let took = start.elapsed().as_millis() as u64;

    // `SELECT GRAPH` yields a single {nodes, edges} object — surface it as `graph`.
    if sql.to_ascii_uppercase().starts_with("SELECT GRAPH") {
        let graph = hits.into_iter().next().and_then(|h| h.payload)
            .unwrap_or_else(|| json!({ "nodes": [], "edges": [] }));
        return HttpResponse::ok(json!({ "ok": true, "graph": graph, "took_ms": took }));
    }
    let rows: Vec<Value> = hits.into_iter().map(|h| h.payload.unwrap_or(Value::Null)).collect();
    let row_count = rows.len();
    HttpResponse::ok(json!({ "ok": true, "rows": rows, "row_count": row_count, "took_ms": took }))
}

fn graph_post(db: &Arc<RwLock<CoreDB>>, body: &[u8], read_only: bool) -> HttpResponse {
    if read_only {
        return HttpResponse::err(403, "read_only", "server is read-only");
    }
    let g: GraphBody = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HttpResponse::err(400, "bad_json", &e.to_string()),
    };
    let start = std::time::Instant::now();

    // Validate + shape nodes up front (flat form: each carries `_collection`+`_key`).
    let mut node_pairs: Vec<(String, String)> = Vec::with_capacity(g.nodes.len());
    for doc in g.nodes {
        let obj = match doc {
            Value::Object(m) => m,
            _ => return HttpResponse::err(400, "bad_document", "each node must be a JSON object"),
        };
        let coll = match obj.get("_collection").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return HttpResponse::err(400, "missing_collection", "each node needs a non-empty string `_collection`"),
        };
        let key = match obj.get("_key").and_then(|v| v.as_str()) {
            Some(k) => k.to_string(),
            None => return HttpResponse::err(400, "missing_key", "each node needs a string `_key`"),
        };
        let json = match serde_json::to_string(&Value::Object(obj)) {
            Ok(j) => j,
            Err(e) => return HttpResponse::err(400, "bad_json", &e.to_string()),
        };
        node_pairs.push((format!("{coll}/{key}"), json));
    }

    let mut edge_rows: Vec<(String, String, String, Option<String>)> = Vec::with_capacity(g.edges.len());
    for e in g.edges {
        let meta = match e.attrs {
            None | Some(Value::Null) => None,
            Some(v @ Value::Object(_)) => Some(v.to_string()),
            Some(_) => return HttpResponse::err(400, "bad_attrs", "edge `attrs` must be a JSON object"),
        };
        edge_rows.push((e.from, e.to, e.ty, meta));
    }

    let mut db = db.write().unwrap_or_else(|e| e.into_inner());
    let nodes = match db.put_many(node_pairs.iter().map(|(s, j)| (s.as_str(), j.as_str()))) {
        Ok(h) => h.len(),
        Err(e) => return HttpResponse::err(400, "insert_error", &e.to_string()),
    };
    let edge_iter = edge_rows.iter().map(|(f, t, ty, m)| (f.as_str(), t.as_str(), ty.as_str(), m.as_deref()));
    if let Err(e) = db.link_meta_many(edge_iter) {
        return HttpResponse::err(400, "edge_error", &e.to_string());
    }
    HttpResponse::ok(json!({
        "ok": true, "nodes": nodes, "edges": edge_rows.len(), "took_ms": start.elapsed().as_millis() as u64
    }))
}

fn graph_delete(db: &Arc<RwLock<CoreDB>>, body: &[u8], read_only: bool) -> HttpResponse {
    if read_only {
        return HttpResponse::err(403, "read_only", "server is read-only");
    }
    let g: GraphBody = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HttpResponse::err(400, "bad_json", &e.to_string()),
    };
    let start = std::time::Instant::now();

    let mut node_slugs: Vec<String> = Vec::with_capacity(g.nodes.len());
    for doc in g.nodes {
        let obj = match doc {
            Value::Object(m) => m,
            _ => return HttpResponse::err(400, "bad_document", "each node must be a JSON object"),
        };
        let coll = match obj.get("_collection").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c,
            _ => return HttpResponse::err(400, "missing_collection", "each node needs a non-empty string `_collection`"),
        };
        let key = match obj.get("_key").and_then(|v| v.as_str()) {
            Some(k) => k,
            None => return HttpResponse::err(400, "missing_key", "each node needs a string `_key`"),
        };
        node_slugs.push(format!("{coll}/{key}"));
    }

    let mut db = db.write().unwrap_or_else(|e| e.into_inner());
    for slug in &node_slugs {
        db.remove(slug);
    }
    let mut unlinked = 0usize;
    for e in &g.edges {
        db.unlink(&e.from, &e.to, &e.ty);
        unlinked += 1;
    }
    HttpResponse::ok(json!({
        "ok": true, "nodes": node_slugs.len(), "edges": unlinked, "took_ms": start.elapsed().as_millis() as u64
    }))
}
