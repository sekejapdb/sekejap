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
    inner: Mutex<Option<CoreDB>>,
}

fn closed() -> Error {
    Error::from_reason("DB is closed")
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
            .map(|c| Db { inner: Mutex::new(Some(c)) })
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Run a mutating statement; returns affected rows.
    #[napi]
    pub fn execute(&self, sql: String) -> Result<i64> {
        let mut guard = self.inner.lock().unwrap();
        let db = guard.as_mut().ok_or_else(closed)?;
        db.execute(&sql)
            .map(|n| n as i64)
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Run a SELECT; returns a JSON-array string (caller `JSON.parse`s it).
    #[napi]
    pub fn query(&self, sql: String) -> Result<String> {
        let guard = self.inner.lock().unwrap();
        let db = guard.as_ref().ok_or_else(closed)?;
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
        let guard = self.inner.lock().unwrap();
        let db = guard.as_ref().ok_or_else(closed)?;
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

    /// Parameterized mutating statement ($1, $2, …); `paramsJson` is a JSON array
    /// string. Returns affected rows. The typed layer's update/delete lower here.
    #[napi]
    pub fn execute_params(&self, sql: String, params_json: String) -> Result<i64> {
        let params: Vec<serde_json::Value> = serde_json::from_str(&params_json)
            .map_err(|e| Error::from_reason(format!("params_json: {e}")))?;
        let mut guard = self.inner.lock().unwrap();
        let db = guard.as_mut().ok_or_else(closed)?;
        db.execute_params(&sql, &params)
            .map(|n| n as i64)
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    // ── Change feed (reactive .watch()) ───────────────────────────────────────

    /// Subscribe to the change feed. `callback` is invoked once per committed
    /// mutation (a transaction fires once, at COMMIT) with a JSON string
    /// `{"collections":[…],"keys":[…],"edge_types":[…]}`. Returns a subscription
    /// id; pass it to [`unwatch`] to stop. The napi ThreadsafeFunction marshals
    /// the call onto the JS event loop, so no manual threading is needed.
    #[napi]
    pub fn watch(
        &self,
        callback: napi::threadsafe_function::ThreadsafeFunction<
            String,
            napi::threadsafe_function::ErrorStrategy::Fatal,
        >,
    ) -> Result<i64> {
        let mut guard = self.inner.lock().unwrap();
        let db = guard.as_mut().ok_or_else(closed)?;
        let id = db.subscribe_changes(move |ev| {
            let json = serde_json::json!({
                "collections": ev.collections,
                "keys": ev.keys,
                "edge_types": ev.edge_types,
            })
            .to_string();
            callback.call(json, napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking);
        });
        Ok(id as i64)
    }

    /// Stop a change-feed subscription created by [`watch`].
    #[napi]
    pub fn unwatch(&self, id: i64) {
        if let Some(db) = self.inner.lock().unwrap().as_mut() {
            db.unsubscribe_changes(id as u64);
        }
    }

    /// Compile a query once for repeated execution — a prepared statement.
    #[napi]
    pub fn prepare(&self, sql: String) -> Result<PreparedStatement> {
        let guard = self.inner.lock().unwrap();
        let db = guard.as_ref().ok_or_else(closed)?;
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
        let guard = self.inner.lock().unwrap();
        let db = guard.as_ref().ok_or_else(closed)?;
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
        let mut guard = self.inner.lock().unwrap();
        let db = guard.as_mut().ok_or_else(closed)?;
        db.put(&slug, &payload_json)
            .map(|_| ())
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Create a plain edge from -> to of the given type.
    #[napi]
    pub fn link(&self, from: String, to: String, edge_type: String) {
        if let Some(db) = self.inner.lock().unwrap().as_mut() { db.link(&from, &to, &edge_type); }
    }

    /// Number of nodes.
    #[napi]
    pub fn node_count(&self) -> i64 {
        self.inner.lock().unwrap().as_ref().map_or(0, |db| db.node_count()) as i64
    }

    /// Number of edges.
    #[napi]
    pub fn edge_count(&self) -> i64 {
        self.inner.lock().unwrap().as_ref().map_or(0, |db| db.edge_count()) as i64
    }

    /// Compact: truncate WAL, rewrite payloads/topology, reclaim RAM.
    #[napi]
    pub fn compact(&self) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .as_mut()
            .ok_or_else(closed)?
            .compact()
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    // ── Open modes ───────────────────────────────────────────────────────────

    /// Open an existing database in paged mode: identity/topology served from
    /// memory-mapped files — small open time and resident memory at any size.
    #[napi(factory)]
    pub fn open_paged(path: String) -> Result<Db> {
        CoreDB::open_paged(std::path::Path::new(&path))
            .map(|c| Db { inner: Mutex::new(Some(c)) })
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Open an existing database read-only.
    #[napi(factory)]
    pub fn open_read_only(path: String) -> Result<Db> {
        CoreDB::open_read_only(std::path::Path::new(&path))
            .map(|c| Db { inner: Mutex::new(Some(c)) })
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    // ── Nodes ────────────────────────────────────────────────────────────────

    /// A node's raw JSON payload, or null.
    #[napi]
    pub fn get(&self, slug: String) -> Option<String> {
        self.inner.lock().unwrap().as_ref().and_then(|db| db.get(&slug))
    }

    /// True if the node exists.
    #[napi]
    pub fn contains(&self, slug: String) -> bool {
        self.inner.lock().unwrap().as_ref().is_some_and(|db| db.contains(&slug))
    }

    /// Delete a node (and its edges).
    #[napi]
    pub fn remove(&self, slug: String) {
        if let Some(db) = self.inner.lock().unwrap().as_mut() { db.remove(&slug); }
    }

    /// Store many nodes in one batch (single disk sync).
    /// `pairsJson` is a JSON array of `[slug, payloadJson]` pairs.
    #[napi]
    pub fn put_many(&self, pairs_json: String) -> Result<i64> {
        let pairs: Vec<(String, String)> = serde_json::from_str(&pairs_json)
            .map_err(|e| Error::from_reason(format!("pairs_json: {e}")))?;
        let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, j)| (k.as_str(), j.as_str())).collect();
        self.inner.lock().unwrap().as_mut().ok_or_else(closed)?.put_many(refs)
            .map(|v| v.len() as i64)
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Begin a bulk-load scope (defer the per-write disk sync). Pair with `endBulk`.
    #[napi]
    pub fn begin_bulk(&self) {
        if let Some(db) = self.inner.lock().unwrap().as_mut() { db.begin_bulk(); }
    }

    /// End a bulk-load scope: one disk sync for the whole batch.
    #[napi]
    pub fn end_bulk(&self) {
        if let Some(db) = self.inner.lock().unwrap().as_mut() { db.end_bulk(); }
    }

    // ── Vectors ──────────────────────────────────────────────────────────────

    /// Store an embedding under a named field of a node.
    #[napi]
    pub fn put_vector(&self, slug: String, field: String, data: Vec<f64>) -> Result<()> {
        let v: Vec<f32> = data.into_iter().map(|x| x as f32).collect();
        self.inner.lock().unwrap().as_mut().ok_or_else(closed)?.put_vector(&slug, &field, &v)
            .map(|_| ())
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// The stored embedding for a node's field, or null.
    #[napi]
    pub fn get_vector(&self, slug: String, field: String) -> Option<Vec<f64>> {
        self.inner.lock().unwrap().as_ref().and_then(|db| db.get_vector(&slug, &field))
            .map(|v| v.into_iter().map(|x| x as f64).collect())
    }

    // ── Edges ────────────────────────────────────────────────────────────────

    /// Create an edge with JSON attributes (primitives ride fast columns).
    #[napi]
    pub fn link_meta(&self, from: String, to: String, edge_type: String, meta_json: String) -> Result<()> {
        self.inner.lock().unwrap().as_mut().ok_or_else(closed)?.link_meta(&from, &to, &edge_type, &meta_json)
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Create many edges in one batch (single disk sync).
    /// `edgesJson` is a JSON array of `[from, to, edgeType]` triples.
    #[napi]
    pub fn link_many(&self, edges_json: String) -> Result<()> {
        let edges: Vec<(String, String, String)> = serde_json::from_str(&edges_json)
            .map_err(|e| Error::from_reason(format!("edges_json: {e}")))?;
        let refs: Vec<(&str, &str, &str)> =
            edges.iter().map(|(f, t, e)| (f.as_str(), t.as_str(), e.as_str())).collect();
        self.inner.lock().unwrap().as_mut().ok_or_else(closed)?.link_many(refs);
        Ok(())
    }

    /// Remove a directed edge.
    #[napi]
    pub fn unlink(&self, from: String, to: String, edge_type: String) {
        if let Some(db) = self.inner.lock().unwrap().as_mut() { db.unlink(&from, &to, &edge_type); }
    }

    /// Remove edges matching attribute equality conditions (`propsJson` is a
    /// JSON object). Returns how many were removed.
    #[napi]
    pub fn unlink_where(&self, from: String, to: String, edge_type: String, props_json: String) -> i64 {
        self.inner.lock().unwrap().as_mut().map_or(0, |db| db.unlink_where(&from, &to, &edge_type, &props_json)) as i64
    }

    /// Update attributes on matching edges: `propsJson` selects, `setsJson`
    /// assigns. Returns how many were updated.
    #[napi]
    pub fn update_edge(&self, from: String, to: String, edge_type: String, props_json: String, sets_json: String) -> i64 {
        self.inner.lock().unwrap().as_mut().map_or(0, |db| db.update_edge(&from, &to, &edge_type, &props_json, &sets_json)) as i64
    }

    /// Edges leaving a node, as a JSON-array string of
    /// `{from, to, type, meta}` objects.
    #[napi]
    pub fn edges_from(&self, slug: String) -> Result<String> {
        edge_hits_json(self.inner.lock().unwrap().as_ref().ok_or_else(closed)?.edges_from(&slug))
    }

    /// Edges arriving at a node, same shape as `edgesFrom`.
    #[napi]
    pub fn edges_to(&self, slug: String) -> Result<String> {
        edge_hits_json(self.inner.lock().unwrap().as_ref().ok_or_else(closed)?.edges_to(&slug))
    }

    /// Edges from one collection to another, same shape as `edgesFrom`.
    #[napi]
    pub fn edges_between(&self, from_collection: String, to_collection: String) -> Result<String> {
        edge_hits_json(self.inner.lock().unwrap().as_ref().ok_or_else(closed)?.edges_between(&from_collection, &to_collection))
    }

    // ── Introspection ────────────────────────────────────────────────────────

    /// All collection names.
    #[napi]
    pub fn collection_names(&self) -> Vec<String> {
        self.inner.lock().unwrap().as_ref().map_or_else(Vec::new, |db| db.collection_names())
    }

    /// Every node slug.
    #[napi]
    pub fn all_slugs(&self) -> Vec<String> {
        self.inner.lock().unwrap().as_ref().map_or_else(Vec::new, |db| db.all_slugs())
    }

    /// DDL string for a collection schema, or null.
    #[napi]
    pub fn schema_ddl(&self, collection: String) -> Option<String> {
        self.inner.lock().unwrap().as_ref().and_then(|db| db.schema_ddl(&collection))
    }

    /// Ranked text search over a BM25-indexed field: JSON-array string of
    /// `[slug, score]` pairs, best first.
    #[napi]
    pub fn bm25_search(&self, field: String, query: String, top_k: i64) -> Result<String> {
        let guard = self.inner.lock().unwrap();
        let db = guard.as_ref().ok_or_else(closed)?;
        let rows: Vec<(String, f64)> = db
            .bm25_search(&field, &query, top_k as usize)
            .into_iter()
            .filter_map(|(h, sc)| db.slug_of(h).map(|s| (s, sc)))
            .collect();
        serde_json::to_string(&rows).map_err(|e| Error::from_reason(e.to_string()))
    }

    /// The query plan for a statement, as a JSON-array string (one step per
    /// element). `analyze = true` also executes it and adds per-step timings.
    #[napi]
    pub fn explain(&self, sql: String, analyze: Option<bool>) -> Result<String> {
        let guard = self.inner.lock().unwrap();
        let db = guard.as_ref().ok_or_else(closed)?;
        let hits = if analyze.unwrap_or(false) {
            db.explain_analyze(&sql).map_err(|e| Error::from_reason(e.to_string()))?
        } else {
            db.explain(&sql).map_err(|e| Error::from_reason(e.to_string()))?
        };
        let rows: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| h.payload.unwrap_or_else(|| serde_json::json!({ "_slug": h.slug })))
            .collect();
        serde_json::to_string(&serde_json::Value::Array(rows))
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Run a SHOW statement (`SHOW TABLES`, `SHOW EDGES`, …); JSON-array string.
    #[napi]
    pub fn show(&self, sql: String) -> Result<String> {
        let guard = self.inner.lock().unwrap();
        let db = guard.as_ref().ok_or_else(closed)?;
        let hits = db.show(&sql).map_err(|e| Error::from_reason(e.to_string()))?;
        let rows: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| h.payload.unwrap_or_else(|| serde_json::json!({ "_slug": h.slug })))
            .collect();
        serde_json::to_string(&serde_json::Value::Array(rows))
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    // ── Maintenance ──────────────────────────────────────────────────────────

    /// Shrink resident memory to the live working set (never drops indexes).
    #[napi]
    pub fn trim_memory(&self) {
        if let Some(db) = self.inner.lock().unwrap().as_mut() { db.trim_memory(); }
    }

    /// Per-structure resident-memory estimate, as a JSON object string.
    #[napi]
    pub fn memory_report(&self) -> Result<String> {
        let map: std::collections::BTreeMap<String, usize> = self
            .inner.lock().unwrap()
            .as_ref().ok_or_else(closed)?
            .memory_report()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        serde_json::to_string(&map).map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Override HNSW search breadth (`efSearch`); null restores the default.
    #[napi]
    pub fn set_hnsw_ef_search(&self, ef: Option<i64>) {
        if let Some(db) = self.inner.lock().unwrap().as_mut() { db.set_hnsw_ef_search(ef.map(|e| e as usize)); }
    }

    /// Close the database and release its lock. Further calls error (or no-op
    /// for count/list getters). Safe to call twice.
    #[napi]
    pub fn close(&self) {
        self.inner.lock().unwrap().take();
    }
}

/// Serialize edge hits to the JSON shape `{from, to, type, meta}`.
fn edge_hits_json(hits: Vec<sekejap::EdgeHit>) -> Result<String> {
    let rows: Vec<serde_json::Value> = hits
        .into_iter()
        .map(|e| serde_json::json!({
            "from": e.from_slug,
            "to": e.to_slug,
            "type": e.edge_type,
            "meta": e.meta,
        }))
        .collect();
    serde_json::to_string(&serde_json::Value::Array(rows))
        .map_err(|e| Error::from_reason(e.to_string()))
}

/// The library version.
#[napi]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
