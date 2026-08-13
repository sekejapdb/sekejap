//! Python bindings for sekejap via PyO3.

use pyo3::exceptions::{PyIOError, PyTypeError};
use pyo3::prelude::*;
use serde_json::Value;

use ::sekejap::CoreDB;
use ::sekejap::Hit;
use ::sekejap::PreparedQuery;

// ── PyHit ─────────────────────────────────────────────────────────────────────

/// A single result row from a query.
///
/// Attributes:
///     slug (str): The node's key path, e.g. ``"students/ali"``.
///     payload (str | None): Raw JSON string of the node payload, or ``None``.
///         Parse with ``json.loads(hit.payload)``.
#[pyclass(name = "Hit")]
#[derive(Clone)]
pub struct PyHit {
    #[pyo3(get)]
    pub slug: String,
    /// Raw JSON string — call json.loads() on the Python side.
    #[pyo3(get)]
    pub payload: Option<String>,
}

/// A prepared (compiled) query — from :meth:`DB.prepare`, run with
/// :meth:`DB.query_prepared`. Opaque; reuse for the same shape with new params.
#[pyclass(name = "PreparedQuery")]
pub struct PyPreparedQuery {
    inner: PreparedQuery,
}

#[pymethods]
impl PyHit {
    fn __repr__(&self) -> String {
        let preview = self.payload.as_deref()
            .map(|s| &s[..s.len().min(80)])
            .unwrap_or("None");
        format!("Hit(slug={:?}, payload={})", self.slug, preview)
    }
}

fn to_pyhit(h: Hit) -> PyHit {
    PyHit {
        slug: h.slug,
        payload: h.payload.as_ref().map(|v| v.to_string()),
    }
}

// ── PyEdgeHit ─────────────────────────────────────────────────────────────────

/// A resolved edge from a graph query or edge inspection call.
///
/// Attributes:
///     from_slug (str | None): Slug of the source node.
///     to_slug (str | None): Slug of the target node.
///     edge_type (str | None): Human-readable edge type label, e.g. ``"route_to"``.
///     meta_json (str | None): Raw JSON string of edge metadata, or ``None``.
///         Parse with ``json.loads(hit.meta_json)``.
#[pyclass(name = "EdgeHit")]
#[derive(Clone)]
pub struct PyEdgeHit {
    #[pyo3(get)]
    pub from_slug: Option<String>,
    #[pyo3(get)]
    pub to_slug: Option<String>,
    #[pyo3(get)]
    pub edge_type: Option<String>,
    #[pyo3(get)]
    pub meta_json: Option<String>,
}

#[pymethods]
impl PyEdgeHit {
    fn __repr__(&self) -> String {
        format!(
            "EdgeHit(from={:?}, to={:?}, type={:?})",
            self.from_slug, self.to_slug, self.edge_type
        )
    }
}

fn to_pyedge(e: ::sekejap::EdgeHit) -> PyEdgeHit {
    PyEdgeHit {
        from_slug: e.from_slug,
        to_slug: e.to_slug,
        edge_type: e.edge_type,
        meta_json: e.meta.map(|m| m.to_string()),
    }
}

fn db_err(e: impl std::fmt::Display) -> PyErr {
    PyIOError::new_err(e.to_string())
}

/// Convert a Python list of values to `Vec<serde_json::Value>`.
fn py_list_to_values(py: Python<'_>, objs: Vec<PyObject>) -> PyResult<Vec<Value>> {
    objs.into_iter().map(|o| {
        // bool must come before i64 (Python bool is a subclass of int)
        if let Ok(b) = o.extract::<bool>(py)   { return Ok(Value::Bool(b)); }
        if let Ok(i) = o.extract::<i64>(py)    { return Ok(serde_json::json!(i)); }
        if let Ok(f) = o.extract::<f64>(py)    { return Ok(serde_json::json!(f)); }
        if let Ok(s) = o.extract::<String>(py) { return Ok(Value::String(s)); }
        if o.is_none(py)                        { return Ok(Value::Null); }
        // Vec<f32> for vector params
        if let Ok(v) = o.extract::<Vec<f32>>(py) {
            return Ok(Value::Array(v.into_iter().map(|f| serde_json::json!(f as f64)).collect()));
        }
        Err(PyTypeError::new_err(
            "parameter must be str, int, float, bool, None, or list[float]"
        ))
    }).collect()
}

// ── PyDB ──────────────────────────────────────────────────────────────────────

/// An embedded graph + document database.
///
/// Example::
///
///     from sekejap import DB
///
///     db = DB()               # in-memory
///     db = DB("./data")       # persistent (WAL-backed)
///
///     db.put("students/ali", '{"_collection":"students","name":"Ali"}')
///     db.link("cls/math", "lec/ali", "taught_by")
///
///     hits = db.query("SELECT * FROM students")
///     for h in hits:
///         print(h.slug, h.payload)   # payload is a JSON string
#[pyclass(name = "DB", subclass)]
pub struct PyDB {
    inner: Option<CoreDB>,
}

#[pymethods]
impl PyDB {
    /// Open or create a database.
    ///
    /// Args:
    ///     path (str, optional): Directory for persistent storage.
    ///         Omit or pass ``None`` for an in-memory database.
    #[new]
    #[pyo3(signature = (path=None))]
    fn new(path: Option<&str>) -> PyResult<Self> {
        let inner = match path {
            Some(p) => CoreDB::open(p).map_err(db_err)?,
            None    => CoreDB::new(),
        };
        Ok(Self { inner: Some(inner) })
    }

    /// Open an existing database in paged mode: identity and topology are
    /// served from memory-mapped files, so open time and resident memory stay
    /// small regardless of database size. Read-heavy workloads only.
    #[staticmethod]
    fn open_paged(path: &str) -> PyResult<Self> {
        Ok(Self { inner: Some(CoreDB::open_paged(std::path::Path::new(path)).map_err(db_err)?) })
    }

    /// Open an existing database read-only (no lock contention with a writer).
    #[staticmethod]
    fn open_read_only(path: &str) -> PyResult<Self> {
        Ok(Self { inner: Some(CoreDB::open_read_only(std::path::Path::new(path)).map_err(db_err)?) })
    }

    // ── Nodes ─────────────────────────────────────────────────────────────────

    /// Store a node. ``json`` must contain ``_collection`` and ``_key``.
    fn put(&mut self, key: &str, json: &str) -> PyResult<()> {
        self.db_mut()?.put(key, json).map(|_| ()).map_err(db_err)
    }

    /// Retrieve a node's raw JSON string, or ``None``.
    fn get(&self, key: &str) -> PyResult<Option<String>> {
        Ok(self.db()?.get(key))
    }

    /// Delete a node (and its edges).
    fn remove(&mut self, key: &str) {
        if let Some(db) = self.inner.as_mut() { db.remove(key); }
    }

    /// Return ``True`` if the node exists.
    fn contains(&self, key: &str) -> PyResult<bool> {
        Ok(self.db()?.contains(key))
    }

    /// Store many nodes in one batch: a list of ``(key, json)`` pairs.
    /// One disk sync for the whole batch — the fast path for bulk loads.
    fn put_many(&mut self, pairs: Vec<(String, String)>) -> PyResult<usize> {
        let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, j)| (k.as_str(), j.as_str())).collect();
        self.db_mut()?.put_many(refs).map(|v| v.len()).map_err(db_err)
    }

    /// Begin a bulk-load scope: defers the per-write disk sync until
    /// :meth:`end_bulk`. Wrap large streams of ``put`` / ``put_vector`` /
    /// ``link`` calls in a bulk scope to avoid one sync per write.
    fn begin_bulk(&mut self) -> PyResult<()> {
        self.db_mut()?.begin_bulk(); Ok(())
    }

    /// End a bulk-load scope: one disk sync for everything since
    /// :meth:`begin_bulk`.
    fn end_bulk(&mut self) -> PyResult<()> {
        self.db_mut()?.end_bulk(); Ok(())
    }

    /// Store a vector under a named field of a node.
    fn put_vector(&mut self, key: &str, field: &str, data: Vec<f32>) -> PyResult<()> {
        self.db_mut()?.put_vector(key, field, &data).map(|_| ()).map_err(db_err)
    }

    /// Retrieve the stored vector for a node's field, or ``None``.
    fn get_vector(&self, key: &str, field: &str) -> PyResult<Option<Vec<f32>>> {
        Ok(self.db()?.get_vector(key, field))
    }

    // ── Edges ─────────────────────────────────────────────────────────────────

    /// Create a directed edge: ``from -[edge_type]-> to``.
    fn link(&mut self, from: &str, to: &str, edge_type: &str) {
        if let Some(db) = self.inner.as_mut() {
            db.link(from, to, edge_type);
        }
    }

    /// Create a directed edge with JSON metadata. Primitive attributes route to
    /// the fast lane; the rest ride the JSON bag.
    fn link_meta(&mut self, from: &str, to: &str, edge_type: &str, meta_json: &str) -> PyResult<()> {
        self.db_mut()?.link_meta(from, to, edge_type, meta_json).map_err(db_err)
    }

    /// Remove a directed edge.
    fn unlink(&mut self, from: &str, to: &str, edge_type: &str) {
        if let Some(db) = self.inner.as_mut() { db.unlink(from, to, edge_type); }
    }

    /// Create many edges in one batch: a list of ``(from, to, edge_type)``.
    /// One disk sync for the whole batch.
    fn link_many(&mut self, edges: Vec<(String, String, String)>) -> PyResult<()> {
        let refs: Vec<(&str, &str, &str)> =
            edges.iter().map(|(f, t, e)| (f.as_str(), t.as_str(), e.as_str())).collect();
        self.db_mut()?.link_many(refs); Ok(())
    }

    /// Remove edges matching attribute values (``props_json`` is a JSON object
    /// of attribute equality conditions). Returns how many were removed.
    fn unlink_where(&mut self, from: &str, to: &str, edge_type: &str, props_json: &str) -> PyResult<usize> {
        Ok(self.db_mut()?.unlink_where(from, to, edge_type, props_json))
    }

    /// Update attributes on matching edges: ``props_json`` selects (equality
    /// conditions), ``sets_json`` assigns. Returns how many were updated.
    fn update_edge(&mut self, from: &str, to: &str, edge_type: &str, props_json: &str, sets_json: &str) -> PyResult<usize> {
        Ok(self.db_mut()?.update_edge(from, to, edge_type, props_json, sets_json))
    }

    /// All edges leaving a node. Each :class:`EdgeHit` has ``from_slug``,
    /// ``to_slug``, ``edge_type``, and ``meta_json`` (attributes as JSON, or ``None``).
    fn edges_from(&self, key: &str) -> PyResult<Vec<PyEdgeHit>> {
        Ok(self.db()?.edges_from(key).into_iter().map(to_pyedge).collect())
    }

    /// All edges arriving at a node.
    fn edges_to(&self, key: &str) -> PyResult<Vec<PyEdgeHit>> {
        Ok(self.db()?.edges_to(key).into_iter().map(to_pyedge).collect())
    }

    /// All edges from one collection to another.
    fn edges_between(&self, from_collection: &str, to_collection: &str) -> PyResult<Vec<PyEdgeHit>> {
        Ok(self.db()?.edges_between(from_collection, to_collection).into_iter().map(to_pyedge).collect())
    }

    // ── SQL ───────────────────────────────────────────────────────────────────

    /// Execute a SQL query. Returns a list of :class:`Hit`.
    ///
    /// Each ``hit.payload`` is a raw JSON string — use ``json.loads(hit.payload)``.
    ///
    /// Supported forms::
    ///
    ///     # Standard SELECT
    ///     db.query("SELECT * FROM dishes WHERE price <= 90000")
    ///
    ///     # Graph aggregate
    ///     db.query("""
    ///         SELECT p._key AS place, COUNT(*) AS visits
    ///         FROM MATCH (p:places)<-[:visited]-(t:tourists)
    ///         GROUP BY p._key ORDER BY visits DESC LIMIT 10
    ///     """)
    ///
    ///     # Multi-stage graph query with WITH chaining
    ///     db.query("""
    ///         SELECT d.name AS dish, COUNT(*) AS orders
    ///         FROM MATCH (c:tourists)-[:similar_taste]->(peer:tourists)
    ///         WHERE c._key = 'chloe'
    ///         WITH peer
    ///         MATCH (peer)-[:ate]->(d:dishes)
    ///         GROUP BY d.name ORDER BY orders DESC LIMIT 10
    ///     """)
    ///
    ///     # SELECT ... FROM MATCH (also via query())
    ///     db.query("""
    ///         SELECT t._key AS tourist, p.name AS place
    ///         FROM MATCH (t:tourists)-[:visited]->(p:places)
    ///     """)
    ///
    ///     # PATH_* aggregates — operate on path intrinsic arrays
    ///     db.query("""
    ///         SELECT c._key AS dest, PATH_PRODUCT(r2.weight) AS reliability
    ///         FROM MATCH (a:places)-[r:route_to]->(b:places)-[r2:route_to]->(c:places)
    ///         WHERE a._key = 'seminyak'
    ///     """)
    ///
    ///     # CASE WHEN
    ///     db.query("""
    ///         SELECT d.name AS dish,
    ///                CASE WHEN d.protein_g >= 30 THEN 'high protein' ELSE 'light' END AS tier
    ///         FROM MATCH (r:restaurants)-[:serves]->(d:dishes)
    ///     """)
    ///
    ///     # MATCH SHORTEST — returns a row with path fields
    ///     db.query("""
    ///         SELECT a.name AS from_name, b.name AS to_name, length(r) AS hops
    ///         FROM MATCH SHORTEST (a)-[r*]->(b)
    ///         WHERE a._key = 'places/seminyak' AND b._key = 'places/uluwatu'
    ///     """)
    ///
    ///     # Multi-FROM cross-join
    ///     db.query("""
    ///         SELECT a._key AS place, b._key AS tourist
    ///         FROM places AS a, MATCH ('tours/sunrise-batur')-[:includes]->(b)
    ///     """)
    ///
    /// Optionally pass ``params`` for ``$1``, ``$2``, … bindings.
    ///
    /// Example::
    ///
    ///     db.query("SELECT * FROM users WHERE name = $1 AND age > $2", ["Alice", 25])
    #[pyo3(signature = (sql, params=None))]
    fn query(&self, py: Python<'_>, sql: &str, params: Option<Vec<PyObject>>) -> PyResult<Vec<PyHit>> {
        let hits: Vec<Hit> = if let Some(p) = params {
            let vals = py_list_to_values(py, p)?;
            self.db()?.query_params(sql, &vals).map_err(db_err)?.collect()
        } else {
            self.db()?.query(sql).map_err(db_err)?.collect()
        };
        Ok(hits.into_iter().map(to_pyhit).collect())
    }

    /// Compile a query once for repeated execution — a **prepared statement**.
    /// Parse the SQL now; run it many times with different ``$1``, ``$2``, … values.
    ///
    /// Example::
    ///
    ///     stmt = db.prepare("SELECT * FROM users WHERE age > $1")
    ///     young = db.query_prepared(stmt, [25])
    ///     old   = db.query_prepared(stmt, [60])
    fn prepare(&self, sql: &str) -> PyResult<PyPreparedQuery> {
        let inner = self.db()?.prepare(sql).map_err(db_err)?;
        Ok(PyPreparedQuery { inner })
    }

    /// Run a prepared statement (from :meth:`prepare`), binding ``$1``, ``$2``, ….
    #[pyo3(signature = (stmt, params=None))]
    fn query_prepared(
        &self,
        py: Python<'_>,
        stmt: &PyPreparedQuery,
        params: Option<Vec<PyObject>>,
    ) -> PyResult<Vec<PyHit>> {
        let vals = match params {
            Some(p) => py_list_to_values(py, p)?,
            None => Vec::new(),
        };
        let hits: Vec<Hit> = self
            .db()?
            .query_prepared(&stmt.inner, &vals)
            .map_err(db_err)?
            .collect();
        Ok(hits.into_iter().map(to_pyhit).collect())
    }

    /// Return the query plan for a ``SELECT`` / ``MATCH`` statement without running it.
    ///
    /// Each hit's ``payload`` is a JSON string describing one plan step. Use
    /// ``analyze=True`` to actually execute the query and include per-step row
    /// counts and timings (``EXPLAIN ANALYZE`` semantics).
    ///
    /// Example::
    ///
    ///     for step in db.explain("SELECT * FROM users WHERE age > 30"):
    ///         print(step.payload)
    #[pyo3(signature = (sql, analyze=false))]
    fn explain(&self, sql: &str, analyze: bool) -> PyResult<Vec<PyHit>> {
        let hits = if analyze {
            self.db()?.explain_analyze(sql).map_err(db_err)?
        } else {
            self.db()?.explain(sql).map_err(db_err)?
        };
        Ok(hits.into_iter().map(to_pyhit).collect())
    }

    /// Execute a mutating statement (INSERT / UPDATE / DELETE / CREATE / DROP).
    ///
    /// Returns the number of rows affected. Optionally pass ``params`` for ``$1``, ``$2``, … bindings.
    ///
    /// Example::
    ///
    ///     db.execute("INSERT INTO users (_key, name, age) VALUES ($1, $2, $3)", ["u1", "Bob", 30])
    #[pyo3(signature = (sql, params=None))]
    fn execute(&mut self, py: Python<'_>, sql: &str, params: Option<Vec<PyObject>>) -> PyResult<usize> {
        if let Some(p) = params {
            let vals = py_list_to_values(py, p)?;
            self.db_mut()?.execute_params(sql, &vals).map_err(db_err)
        } else {
            self.db_mut()?.execute(sql).map_err(db_err)
        }
    }

    /// Execute a ``SHOW`` introspection statement.
    ///
    /// Supported forms::
    ///
    ///     db.show("SHOW TABLES")                      # [{name, count}, ...]
    ///     db.show("SHOW EDGES")                       # [{from, type, to, count}, ...]
    ///     db.show("SHOW EDGES FROM collection")       # [{from, type, count}, ...]
    ///     db.show("SHOW EDGES FROM col1 TO col2")     # [{from, type, to, count}, ...]
    ///     db.show("SHOW collection")                  # [{field, type, source, ...}, ...]
    ///
    /// Each hit's ``payload`` is a JSON string — use ``json.loads(hit.payload)``.
    fn show(&self, sql: &str) -> PyResult<Vec<PyHit>> {
        Ok(self.db()?.show(sql).map_err(db_err)?
            .into_iter().map(to_pyhit).collect())
    }

    // ── Introspection ─────────────────────────────────────────────────────────

    /// List all collection names in the database.
    fn collection_names(&self) -> PyResult<Vec<String>> {
        Ok(self.db()?.collection_names())
    }

    /// Return distinct ``(from_collection, edge_type, to_collection)`` triples.
    fn edge_schema(&self) -> PyResult<Vec<(String, String, String)>> {
        Ok(self.db()?.edge_schema())
    }

    /// Return distinct edge type names leaving a collection.
    fn edge_types_from_collection(&self, collection: &str) -> PyResult<Vec<String>> {
        Ok(self.db()?.edge_types_from_collection(collection))
    }

    /// DDL string for a collection schema, or ``None``.
    fn schema_ddl(&self, collection: &str) -> PyResult<Option<String>> {
        Ok(self.db()?.schema_ddl(collection))
    }

    /// Total number of nodes.
    fn node_count(&self) -> PyResult<usize> { Ok(self.db()?.node_count()) }

    /// Total number of edges.
    fn edge_count(&self) -> PyResult<usize> { Ok(self.db()?.edge_count()) }

    /// Every node key in the database.
    fn all_slugs(&self) -> PyResult<Vec<String>> {
        Ok(self.db()?.all_slugs())
    }

    /// Ranked text search over a BM25-indexed field. Returns
    /// ``(key, score)`` pairs, best first.
    fn bm25_search(&self, field: &str, query: &str, top_k: usize) -> PyResult<Vec<(String, f64)>> {
        let db = self.db()?;
        Ok(db.bm25_search(field, query, top_k)
            .into_iter()
            .filter_map(|(h, s)| db.slug_of(h).map(|slug| (slug.to_string(), s)))
            .collect())
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    /// Flush WAL snapshot and truncate the log.
    fn compact(&mut self) -> PyResult<()> {
        self.db_mut()?.compact().map_err(db_err)
    }

    /// Shrink resident memory to the live working set (also runs inside
    /// :meth:`compact`). Never drops indexes.
    fn trim_memory(&mut self) -> PyResult<()> {
        self.db_mut()?.trim_memory(); Ok(())
    }

    /// Per-structure resident-memory estimate: ``{name: bytes}``.
    fn memory_report(&self) -> PyResult<std::collections::HashMap<String, usize>> {
        Ok(self.db()?.memory_report().into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// Override HNSW search breadth (``ef_search``). Higher = better recall,
    /// slower queries. ``None`` restores the per-query default.
    #[pyo3(signature = (ef=None))]
    fn set_hnsw_ef_search(&mut self, ef: Option<usize>) -> PyResult<()> {
        self.db_mut()?.set_hnsw_ef_search(ef); Ok(())
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    fn close(&mut self) { self.inner.take(); }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> { slf }
    fn __exit__(&mut self, _et: PyObject, _ev: PyObject, _tb: PyObject) -> bool {
        self.close();
        false
    }

    fn __repr__(&self) -> String {
        if self.inner.is_some() { "DB(open)".into() } else { "DB(closed)".into() }
    }
}

impl PyDB {
    fn db(&self) -> PyResult<&CoreDB> {
        self.inner.as_ref().ok_or_else(|| PyIOError::new_err("DB is closed"))
    }
    fn db_mut(&mut self) -> PyResult<&mut CoreDB> {
        self.inner.as_mut().ok_or_else(|| PyIOError::new_err("DB is closed"))
    }
}

// ── Module ────────────────────────────────────────────────────────────────────

#[pymodule]
fn sekejap(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDB>()?;
    m.add_class::<PyHit>()?;
    m.add_class::<PyEdgeHit>()?;
    m.add_class::<PyPreparedQuery>()?;
    Ok(())
}
