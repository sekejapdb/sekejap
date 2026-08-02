//! `sekejap serve` — HTTP/JSON server (behind the `serve` feature).
//!
//! **Thin I/O adapter.** This file owns only the transport: the socket, the tokio
//! runtime, bearer auth, and the axum routing table. Every request→response
//! decision lives in the sans-IO [`sekejap::serve::handle`], so any other
//! language or framework can host the identical surface by feeding it
//! `(method, path, body)`. See `docs/design/serve.md`.

use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::json;
use sekejap::serve::{handle, HttpRequest};
use sekejap::CoreDB;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
struct AppState {
    // Shared handle. `serve::handle` takes a read lock for SELECT/MATCH/SHOW and a
    // write lock for DML/DDL, so reads run concurrently. `std::sync::RwLock` (not
    // tokio's) because the sans-IO service is synchronous — we call it under
    // `spawn_blocking`, which is the correct place for blocking DB work anyway.
    db: Arc<RwLock<CoreDB>>,
    key: Option<String>,
    read_only: bool,
}

/// Entry point for `sekejap serve <db-path> [flags]`. Returns a process exit code.
pub fn run(args: &[String]) -> i32 {
    let mut path: Option<String> = None;
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 5918;
    let mut key: Option<String> = std::env::var("SEKEJAP_KEY").ok().filter(|s| !s.is_empty());
    let mut read_only = false;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => if let Some(v) = it.next() { host = v.clone(); },
            "--port" => if let Some(v) = it.next() {
                match v.parse() { Ok(p) => port = p, Err(_) => { eprintln!("invalid --port {v}"); return 2; } }
            },
            "--key" => key = it.next().cloned(),
            "--read-only" => read_only = true,
            "--help" | "-h" => { print_serve_usage(); return 0; }
            other => {
                if !other.starts_with("--") && path.is_none() {
                    path = Some(other.to_string());
                } else {
                    eprintln!("unexpected argument: {other}");
                    return 2;
                }
            }
        }
    }

    // Safety: exposing beyond localhost without a key is refused.
    let is_local = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    if !is_local && key.is_none() {
        eprintln!(
            "refusing to bind {host} publicly without a key — pass --key <k> or set SEKEJAP_KEY."
        );
        return 2;
    }

    let db = match &path {
        Some(p) => match CoreDB::open(p) {
            Ok(d) => d,
            Err(e) => { eprintln!("failed to open {p}: {e}"); return 1; }
        },
        None => CoreDB::new(),
    };

    let auth_on = key.is_some();
    let label = path.clone().unwrap_or_else(|| ":memory:".to_string());
    let state = AppState { db: Arc::new(RwLock::new(db)), key, read_only };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => { eprintln!("failed to start runtime: {e}"); return 1; }
    };

    rt.block_on(async move {
        // One fallback handler for every route/method — the lib's `handle` does the
        // routing (and returns 404 for unknown paths). The CLI just moves bytes.
        let app = Router::new().fallback(dispatch).with_state(state);

        let addr = format!("{host}:{port}");
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => { eprintln!("failed to bind {addr}: {e}"); return 1; }
        };
        eprintln!(
            "sekejap serving {label} on http://{addr}  (auth: {}, {})",
            if auth_on { "on" } else { "off" },
            if read_only { "read-only" } else { "read-write" },
        );
        match axum::serve(listener, app).await {
            Ok(()) => 0,
            Err(e) => { eprintln!("server error: {e}"); 1 }
        }
    })
}

fn print_serve_usage() {
    println!(
        "sekejap serve <db-path> [flags]\n\
         \n\
         Flags:\n\
         \x20 --host <addr>   bind address (default 127.0.0.1; public bind requires --key)\n\
         \x20 --port <n>      port (default 5918)\n\
         \x20 --key <k>       master API key (or set SEKEJAP_KEY); required to bind publicly\n\
         \x20 --read-only     reject INSERT/UPDATE/DELETE/DDL\n\
         \n\
         Endpoints:\n\
         \x20 POST /query                             {{\"sql\": \"...\", \"params\": [...]}}  -> rows | graph\n\
         \x20 POST /graph                              {{nodes:[...], edges:[...]}}  upsert nodes and/or edges\n\
         \x20 DELETE /graph                            {{nodes:[...], edges:[...]}}  remove nodes and/or edges\n\
         \x20 GET  /health\n\
         \x20 GET  /version"
    );
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

/// The only handler. Extracts `(method, path, body)`, checks auth, then runs the
/// sans-IO `serve::handle` on the blocking pool (DB work is synchronous +
/// CPU-bound, so it must not sit on an async worker). `/health` and `/version`
/// stay open (no auth) so load balancers can probe them.
async fn dispatch(
    State(st): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let open = matches!((method.as_str(), uri.path()), ("GET", "/health") | ("GET", "/version"));
    if !open {
        if let Err(r) = auth(&st, &headers) {
            return r;
        }
    }

    let db = st.db.clone();
    let read_only = st.read_only;
    let m = method.as_str().to_string();
    let path = uri.path().to_string();

    let resp = tokio::task::spawn_blocking(move || {
        let req = HttpRequest { method: &m, path: &path, body: body.as_ref() };
        handle(&db, &req, read_only)
    })
    .await;

    match resp {
        Ok(r) => (
            StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(header::CONTENT_TYPE, "application/json")],
            r.body,
        )
            .into_response(),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "request handler failed"),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Bearer-token check. No configured key = open (localhost dev convenience).
fn auth(st: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(k) = &st.key else { return Ok(()); };
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map_or(false, |t| t == k);
    if ok {
        Ok(())
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "unauthorized", "missing or invalid bearer key"))
    }
}

fn err(code: StatusCode, ecode: &str, msg: &str) -> Response {
    (code, Json(json!({ "ok": false, "error": { "code": ecode, "message": msg } }))).into_response()
}
