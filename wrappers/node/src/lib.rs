//! Node.js binding for sekejap via napi-rs (a native N-API addon).
//!
//! Build (produces a `.node` addon): `npx @napi-rs/cli build --release`, or with
//! plain cargo: `cargo build --release` then load the resulting cdylib as `.node`.

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::Mutex;

use sekejap::CoreDB;

/// An open sekejap database. JS is single-threaded, but we guard with a Mutex so
/// the handle is sound even if shared via worker threads.
#[napi]
pub struct Db {
    inner: Mutex<CoreDB>,
}

/// A prepared (compiled) query — from `Db.prepare`, run with `Db.queryPrepared`.
#[napi]
pub struct PreparedStatement {
    inner: sekejap::PreparedQuery,
}

#[napi]
impl Db {
    /// Open (or create) a database at `path`.
    #[napi(factory)]
    pub fn open(path: String) -> Result<Db> {
        CoreDB::open(&path)
            .map(|c| Db { inner: Mutex::new(c) })
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Run a mutating statement; returns affected rows.
    #[napi]
    pub fn execute(&self, sql: String) -> Result<i64> {
        let mut db = self.inner.lock().unwrap();
        db.execute(&sql)
            .map(|n| n as i64)
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Run a SELECT; returns a JSON-array string (caller `JSON.parse`s it).
    #[napi]
    pub fn query(&self, sql: String) -> Result<String> {
        let db = self.inner.lock().unwrap();
        let set = db.query(&sql).map_err(|e| Error::from_reason(e.to_string()))?;
        let rows: Vec<serde_json::Value> = set
            .collect()
            .into_iter()
            .map(|h| h.payload.unwrap_or_else(|| serde_json::json!({ "_slug": h.slug })))
            .collect();
        serde_json::to_string(&serde_json::Value::Array(rows))
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Parameterized SELECT ($1, $2, …); `paramsJson` is a JSON array string.
    #[napi]
    pub fn query_params(&self, sql: String, params_json: String) -> Result<String> {
        let params: Vec<serde_json::Value> = serde_json::from_str(&params_json)
            .map_err(|e| Error::from_reason(format!("params_json: {e}")))?;
        let db = self.inner.lock().unwrap();
        let set = db
            .query_params(&sql, &params)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let rows: Vec<serde_json::Value> = set
            .collect()
            .into_iter()
            .map(|h| h.payload.unwrap_or_else(|| serde_json::json!({ "_slug": h.slug })))
            .collect();
        serde_json::to_string(&serde_json::Value::Array(rows))
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Compile a query once for repeated execution — a prepared statement.
    #[napi]
    pub fn prepare(&self, sql: String) -> Result<PreparedStatement> {
        let db = self.inner.lock().unwrap();
        db.prepare(&sql)
            .map(|inner| PreparedStatement { inner })
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Run a prepared statement, binding `$1`, `$2`, … from a JSON-array string.
    /// Returns a JSON-array string (caller `JSON.parse`s it).
    #[napi]
    pub fn query_prepared(&self, stmt: &PreparedStatement, params_json: String) -> Result<String> {
        let params: Vec<serde_json::Value> = serde_json::from_str(&params_json)
            .map_err(|e| Error::from_reason(format!("params_json: {e}")))?;
        let db = self.inner.lock().unwrap();
        let set = db
            .query_prepared(&stmt.inner, &params)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let rows: Vec<serde_json::Value> = set
            .collect()
            .into_iter()
            .map(|h| h.payload.unwrap_or_else(|| serde_json::json!({ "_slug": h.slug })))
            .collect();
        serde_json::to_string(&serde_json::Value::Array(rows))
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Insert/replace one node by slug with a JSON payload.
    #[napi]
    pub fn put(&self, slug: String, payload_json: String) -> Result<()> {
        let mut db = self.inner.lock().unwrap();
        db.put(&slug, &payload_json)
            .map(|_| ())
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Create a plain edge from -> to of the given type.
    #[napi]
    pub fn link(&self, from: String, to: String, edge_type: String) {
        self.inner.lock().unwrap().link(&from, &to, &edge_type);
    }

    /// Number of nodes.
    #[napi]
    pub fn node_count(&self) -> i64 {
        self.inner.lock().unwrap().node_count() as i64
    }

    /// Number of edges.
    #[napi]
    pub fn edge_count(&self) -> i64 {
        self.inner.lock().unwrap().edge_count() as i64
    }

    /// Compact: truncate WAL, rewrite payloads/topology, reclaim RAM.
    #[napi]
    pub fn compact(&self) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .compact()
            .map_err(|e| Error::from_reason(e.to_string()))
    }
}

/// The library version.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
