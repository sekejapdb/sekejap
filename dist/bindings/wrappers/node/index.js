'use strict';

// sekejap for Node.js — a thin binding over the C ABI (docs/dist/C_ABI.md),
// via koffi (github.com/Koromix/koffi), NOT napi-rs. No native compile step:
// koffi ships prebuilt N-API glue for its own supported platforms, and this
// file loads the sekejap library itself with a plain dlopen/LoadLibrary at
// require() time. See "Choosing the FFI route" in README.md for why.

const fs = require('fs');
const path = require('path');
const koffi = require('koffi');

// ─────────────────────────────────────────────────────────────────────────
// 1. Locate and load libsekejap
// ─────────────────────────────────────────────────────────────────────────

const LIB_NAMES = {
  darwin: 'libsekejap.dylib',
  linux: 'libsekejap.so',
  win32: 'sekejap.dll',
};

function platformDir() {
  const { platform, arch } = process;
  return `${platform}-${arch}`;
}

function resolveLibPath() {
  // 1. An explicit full path wins outright.
  if (process.env.SEKEJAP_LIB_PATH) return process.env.SEKEJAP_LIB_PATH;

  const filename = LIB_NAMES[process.platform];
  if (!filename) {
    throw new Error(`sekejap: unsupported platform '${process.platform}'`);
  }

  // 2. A directory holding just the library (what a prebuilt
  //    /path/to/libsekejap/ directory looks like, and what
  //    build-native-libs's tarball unpacks to as lib/).
  const dirs = [];
  if (process.env.SEKEJAP_LIB_DIR) dirs.push(process.env.SEKEJAP_LIB_DIR);
  // 3. A copy bundled inside the package at publish time, one directory
  //    per platform-arch (this is what publish-node assembles).
  dirs.push(path.join(__dirname, 'native', platformDir(), 'lib'));
  dirs.push(path.join(__dirname, 'native', platformDir()));

  for (const dir of dirs) {
    const candidate = path.join(dir, filename);
    if (fs.existsSync(candidate)) return candidate;
  }

  // 4. Fall back to the bare filename: the OS loader's own search path
  //    (DYLD_LIBRARY_PATH / LD_LIBRARY_PATH / PATH) may still find it.
  return filename;
}

const libPath = resolveLibPath();
let lib;
try {
  lib = koffi.load(libPath);
} catch (e) {
  throw new Error(
    `sekejap: failed to load libsekejap from '${libPath}'. Set SEKEJAP_LIB_PATH ` +
      `to the full path of libsekejap.{dylib,so,dll}, or SEKEJAP_LIB_DIR to the ` +
      `directory containing it. Original error: ${e.message}`
  );
}

// ─────────────────────────────────────────────────────────────────────────
// 2. Type declarations — mirrors dist/ffi/include/sekejap.h
// ─────────────────────────────────────────────────────────────────────────

// Opaque handles. C never lets us see inside them; koffi.opaque() reserves a
// named struct tag we can take pointers to.
koffi.opaque('SekejapDb');
koffi.opaque('SekejapStmt');
koffi.opaque('SekejapTx');
koffi.opaque('SekejapScan');

// The two closed enumerations, both int32_t on the wire (sekejap.h).
koffi.alias('SekejapStatus', 'int32_t');
koffi.alias('SekejapDirection', 'int32_t');

// Every char* this library RETURNS is heap-allocated and must be freed
// exactly once with sekejap_string_free — except sekejap_version, which is
// static. `HeapStr` is a disposable type: koffi copies the C string into a
// JS string, then calls sekejap_string_free(originalPointer) once, so every
// binding below that returns HeapStr gets automatic, exactly-once freeing
// with no manual bookkeeping.
const stringFree = lib.func('void sekejap_string_free(void *s)');
const HeapStr = koffi.disposable('HeapStr', 'str', stringFree);

// ─────────────────────────────────────────────────────────────────────────
// 3. Function table — one C prototype per entry, copied from sekejap.h
// ─────────────────────────────────────────────────────────────────────────

const fn = {
  // 4.1 Opening and identity
  sekejap_open: lib.func('SekejapDb *sekejap_open(const char *path)'),
  sekejap_open_with_config: lib.func(
    'SekejapDb *sekejap_open_with_config(const char *path, const char *config_json)'
  ),
  sekejap_open_service: lib.func('SekejapDb *sekejap_open_service(const char *path)'),
  sekejap_close: lib.func('void sekejap_close(SekejapDb *db)'),
  sekejap_version: lib.func('const char *sekejap_version(void)'), // NOT HeapStr: static
  sekejap_format_version: lib.func('int32_t sekejap_format_version(void)'),

  // 4.2 Errors and memory
  sekejap_last_error: lib.func('HeapStr sekejap_last_error(const SekejapDb *db)'),
  sekejap_last_error_code: lib.func('SekejapStatus sekejap_last_error_code(const SekejapDb *db)'),

  // 4.3 Documents
  sekejap_put: lib.func(
    'int32_t sekejap_put(SekejapDb *db, const char *collection, const char *key, const char *document_json)'
  ),
  sekejap_put_many: lib.func(
    'long sekejap_put_many(SekejapDb *db, const char *collection, const char *rows_json)'
  ),
  sekejap_get: lib.func('HeapStr sekejap_get(SekejapDb *db, const char *collection, const char *key)'),
  sekejap_exists: lib.func('int32_t sekejap_exists(SekejapDb *db, const char *collection, const char *key)'),
  sekejap_delete: lib.func('int32_t sekejap_delete(SekejapDb *db, const char *collection, const char *key)'),
  sekejap_scan_open: lib.func(
    'SekejapScan *sekejap_scan_open(SekejapDb *db, const char *collection, uintptr_t page_rows)'
  ),
  sekejap_scan_next: lib.func('HeapStr sekejap_scan_next(SekejapScan *scan)'),
  sekejap_scan_close: lib.func('void sekejap_scan_close(SekejapScan *scan)'),

  // 4.4 SQL
  sekejap_execute: lib.func('long sekejap_execute(SekejapDb *db, const char *sql, const char *params_json)'),
  sekejap_query: lib.func('HeapStr sekejap_query(SekejapDb *db, const char *sql, const char *params_json)'),
  sekejap_explain: lib.func('HeapStr sekejap_explain(SekejapDb *db, const char *sql, const char *params_json)'),
  sekejap_prepare: lib.func('SekejapStmt *sekejap_prepare(SekejapDb *db, const char *sql)'),
  sekejap_stmt_query: lib.func('HeapStr sekejap_stmt_query(SekejapStmt *stmt, const char *params_json)'),
  sekejap_stmt_execute: lib.func('long sekejap_stmt_execute(SekejapStmt *stmt, const char *params_json)'),
  sekejap_stmt_rebindable: lib.func('int32_t sekejap_stmt_rebindable(const SekejapStmt *stmt)'),
  sekejap_stmt_free: lib.func('void sekejap_stmt_free(SekejapStmt *stmt)'),
  sekejap_query_open: lib.func(
    'SekejapScan *sekejap_query_open(SekejapDb *db, const char *sql, const char *params_json, uintptr_t page_rows)'
  ),
  sekejap_query_next: lib.func('HeapStr sekejap_query_next(SekejapScan *scan)'),
  sekejap_query_close: lib.func('void sekejap_query_close(SekejapScan *scan)'),

  // 4.5 Edges
  sekejap_link: lib.func(
    'int32_t sekejap_link(SekejapDb *db, const char *from_collection, const char *from_key, const char *edge_type, const char *to_collection, const char *to_key)'
  ),
  sekejap_link_with: lib.func(
    'int32_t sekejap_link_with(SekejapDb *db, const char *from_collection, const char *from_key, const char *edge_type, const char *to_collection, const char *to_key, const char *properties_json)'
  ),
  sekejap_unlink: lib.func(
    'int32_t sekejap_unlink(SekejapDb *db, const char *from_collection, const char *from_key, const char *edge_type, const char *to_collection, const char *to_key)'
  ),
  sekejap_neighbours: lib.func(
    'HeapStr sekejap_neighbours(SekejapDb *db, const char *collection, const char *key, const char *edge_type, SekejapDirection direction, uintptr_t limit)'
  ),

  // 4.6 The catalog
  sekejap_create_collection: lib.func(
    'int32_t sekejap_create_collection(SekejapDb *db, const char *name, const char *fields_json)'
  ),
  sekejap_drop_collection: lib.func('int32_t sekejap_drop_collection(SekejapDb *db, const char *name)'),
  sekejap_collections: lib.func('HeapStr sekejap_collections(SekejapDb *db)'),
  sekejap_describe: lib.func('HeapStr sekejap_describe(SekejapDb *db, const char *collection)'),
  sekejap_count_rows: lib.func('long sekejap_count_rows(SekejapDb *db, const char *collection)'),
  sekejap_scan_count_rows: lib.func('long sekejap_scan_count_rows(SekejapDb *db, const char *collection)'),
  sekejap_scan_count_edges: lib.func('long sekejap_scan_count_edges(SekejapDb *db)'),

  // 4.7 Transactions
  sekejap_tx_begin: lib.func('SekejapTx *sekejap_tx_begin(SekejapDb *db)'),
  sekejap_tx_put: lib.func(
    'int32_t sekejap_tx_put(SekejapTx *tx, const char *collection, const char *key, const char *document_json)'
  ),
  sekejap_tx_delete: lib.func('int32_t sekejap_tx_delete(SekejapTx *tx, const char *collection, const char *key)'),
  sekejap_tx_link: lib.func(
    'int32_t sekejap_tx_link(SekejapTx *tx, const char *from_collection, const char *from_key, const char *edge_type, const char *to_collection, const char *to_key)'
  ),
  sekejap_tx_execute: lib.func('long sekejap_tx_execute(SekejapTx *tx, const char *sql, const char *params_json)'),
  sekejap_tx_commit: lib.func('int32_t sekejap_tx_commit(SekejapTx *tx)'),
  sekejap_tx_rollback: lib.func('int32_t sekejap_tx_rollback(SekejapTx *tx)'),

  // 4.8 Maintenance
  sekejap_checkpoint: lib.func('int32_t sekejap_checkpoint(SekejapDb *db)'),
  sekejap_publish: lib.func('int32_t sekejap_publish(SekejapDb *db)'),
  sekejap_storage: lib.func('HeapStr sekejap_storage(SekejapDb *db)'),

  // 4.9 Service mode
  sekejap_statement_timeout_ms: lib.func(
    'int32_t sekejap_statement_timeout_ms(SekejapDb *db, uint64_t milliseconds)'
  ),
  sekejap_cancel: lib.func('int32_t sekejap_cancel(SekejapDb *db)'),
  sekejap_clear_interrupt: lib.func('int32_t sekejap_clear_interrupt(SekejapDb *db)'),
  sekejap_subscribe: lib.func('long sekejap_subscribe(SekejapDb *db)'),
  sekejap_next_change: lib.func(
    'HeapStr sekejap_next_change(SekejapDb *db, long subscription, uint64_t timeout_ms)'
  ),
  sekejap_unsubscribe: lib.func('int32_t sekejap_unsubscribe(SekejapDb *db, long subscription)'),

  // 4.10 Refused by name (still bound, so the refusal reaches JS with a
  // named error rather than "not a function")
  sekejap_open_memory: lib.func('SekejapDb *sekejap_open_memory(void)'),
  sekejap_trim_memory: lib.func('int32_t sekejap_trim_memory(SekejapDb *db)'),
  sekejap_compact: lib.func('int32_t sekejap_compact(SekejapDb *db)'),
  sekejap_show: lib.func('HeapStr sekejap_show(SekejapDb *db, const char *statement)'),
};

// ─────────────────────────────────────────────────────────────────────────
// 4. Status / direction enums, mirrored as plain JS objects
// ─────────────────────────────────────────────────────────────────────────

const SekejapStatus = Object.freeze({
  Ok: 0,
  Refused: 1,
  Corrupt: 2,
  Unsupported: 3,
  Io: 4,
  Invalid: 5,
  Busy: 6,
  UnknownRow: 7,
  Unknown: 8,
});

const STATUS_NAMES = Object.freeze(
  Object.fromEntries(Object.entries(SekejapStatus).map(([name, code]) => [code, name]))
);

const SekejapDirection = Object.freeze({ Outgoing: 0, Incoming: 1, Both: 2 });

const DIRECTION_CODES = {
  outgoing: SekejapDirection.Outgoing,
  incoming: SekejapDirection.Incoming,
  both: SekejapDirection.Both,
};

function directionCode(direction) {
  if (typeof direction === 'number') return direction;
  const code = DIRECTION_CODES[String(direction || 'both').toLowerCase()];
  if (code === undefined) throw new RangeError(`sekejap: unknown direction '${direction}'`);
  return code;
}

const SEKEJAP_REBIND_UNBOUND = 2;

// ─────────────────────────────────────────────────────────────────────────
// 5. Errors
// ─────────────────────────────────────────────────────────────────────────

class SekejapError extends Error {
  constructor(message, code, status) {
    super(message);
    this.name = 'SekejapError';
    this.code = code; // e.g. "Invalid"
    this.status = status; // e.g. 5
  }
}

// sekejap_last_error/_code are THREAD-LOCAL and take the handle only for
// 0.16 source compatibility, ignoring it — a failed open with no handle
// still reports (docs/dist/C_ABI.md §1). We always pass null-or-handle.
function lastError(handle) {
  const status = fn.sekejap_last_error_code(handle ?? null);
  const message = fn.sekejap_last_error(handle ?? null);
  const code = STATUS_NAMES[status] ?? 'Unknown';
  return new SekejapError(message || `sekejap call failed (${code})`, code, status);
}

function isNegOne(v) {
  return v === -1 || v === -1n;
}

// ─────────────────────────────────────────────────────────────────────────
// 6. JSON boundary helpers
// ─────────────────────────────────────────────────────────────────────────

// "NULL or an empty string is no parameters; any other single JSON value is
// a one-element list" (docs/dist/C_ABI.md §2) — an array is passed through,
// a bare scalar becomes that one-element list on the C side.
function paramsJson(params) {
  if (params === undefined || params === null) return null;
  return JSON.stringify(params);
}

function docJson(doc) {
  return JSON.stringify(doc === undefined ? null : doc);
}

// ─────────────────────────────────────────────────────────────────────────
// 7. Scan — a paged walk, shared by Db.scan (collection order) and
//    Db.stream (a statement's answer). The two close/next pairs are named
//    differently in the ABI but documented as the same operation.
// ─────────────────────────────────────────────────────────────────────────

class Scan {
  #handle;
  #db;
  #close;
  #next;
  #done = false;

  constructor(handle, db, kind) {
    this.#handle = handle;
    this.#db = db;
    if (kind === 'query') {
      this.#next = fn.sekejap_query_next;
      this.#close = fn.sekejap_query_close;
    } else {
      this.#next = fn.sekejap_scan_next;
      this.#close = fn.sekejap_scan_close;
    }
  }

  /** The next page (an array of documents, or of row objects for a stream), or `null` at the end. */
  next() {
    if (this.#done || !this.#handle) return null;
    const json = this.#next(this.#handle);
    if (json === null) {
      const status = fn.sekejap_last_error_code(this.#db._raw());
      this.#done = true;
      if (status !== SekejapStatus.Ok) throw lastError(this.#db._raw());
      return null;
    }
    return JSON.parse(json);
  }

  /** Close the walk. Null-safe; safe to call more than once. */
  close() {
    if (this.#handle) {
      this.#close(this.#handle);
      this.#handle = null;
      this.#done = true;
    }
  }

  /** Iterate pages: `for (const page of scan) ...`. */
  *[Symbol.iterator]() {
    let page;
    // eslint-disable-next-line no-cond-assign
    while ((page = this.next()) !== null) yield page;
  }

  /** Iterate individual rows/documents, flattening every page. */
  *rows() {
    for (const page of this) yield* page;
  }
}

// ─────────────────────────────────────────────────────────────────────────
// 8. Statement — sekejap_prepare / sekejap_stmt_*
// ─────────────────────────────────────────────────────────────────────────

class Statement {
  #handle;
  #db;

  constructor(handle, db) {
    this.#handle = handle;
    this.#db = db;
  }

  #ensureOpen() {
    if (!this.#handle) throw new Error('sekejap: statement is closed');
    return this.#handle;
  }

  /** Run this statement as a row-returning one. Answer: array of row objects. */
  query(params) {
    const json = fn.sekejap_stmt_query(this.#ensureOpen(), paramsJson(params));
    if (json === null) throw lastError(this.#db._raw());
    return JSON.parse(json);
  }

  /** Run this statement as a writing one and commit. Returns rows moved. */
  execute(params) {
    const rc = fn.sekejap_stmt_execute(this.#ensureOpen(), paramsJson(params));
    if (isNegOne(rc)) throw lastError(this.#db._raw());
    return rc;
  }

  /**
   * Whether a further bind compiles nothing: `true`/`false`, or `null` when
   * the statement has not been bound yet (SEKEJAP_REBIND_UNBOUND).
   */
  rebindable() {
    const rc = fn.sekejap_stmt_rebindable(this.#ensureOpen());
    if (rc === SEKEJAP_REBIND_UNBOUND) return null;
    if (isNegOne(rc)) throw lastError(this.#db._raw());
    return rc === 1;
  }

  /** Free the statement. Null-safe; must happen before the database closes. */
  close() {
    if (this.#handle) {
      fn.sekejap_stmt_free(this.#handle);
      this.#handle = null;
    }
  }
}

// ─────────────────────────────────────────────────────────────────────────
// 9. Tx — sekejap_tx_*. Holds the writer for its whole life.
// ─────────────────────────────────────────────────────────────────────────

class Tx {
  #handle;
  #db;

  constructor(handle, db) {
    this.#handle = handle;
    this.#db = db;
  }

  #ensureOpen() {
    if (!this.#handle) throw new Error('sekejap: transaction is already committed or rolled back');
    return this.#handle;
  }

  /** Write one document inside the transaction. Not committed until commit(). */
  put(collection, key, doc) {
    const body = { ...doc, _key: (doc && doc._key) ?? key };
    const rc = fn.sekejap_tx_put(this.#ensureOpen(), collection, key, docJson(body));
    if (rc !== 0) throw lastError(this.#db._raw());
  }

  /** Delete one row inside the transaction. Returns whether it was there. */
  delete(collection, key) {
    const rc = fn.sekejap_tx_delete(this.#ensureOpen(), collection, key);
    if (isNegOne(rc)) throw lastError(this.#db._raw());
    return rc === 1;
  }

  /** Link two rows inside the transaction. */
  link(fromCollection, fromKey, edgeType, toCollection, toKey) {
    const rc = fn.sekejap_tx_link(this.#ensureOpen(), fromCollection, fromKey, edgeType, toCollection, toKey);
    if (rc !== 0) throw lastError(this.#db._raw());
  }

  /** Run one writing statement inside the transaction. Returns rows moved. */
  execute(sql, params) {
    const rc = fn.sekejap_tx_execute(this.#ensureOpen(), sql, paramsJson(params));
    if (isNegOne(rc)) throw lastError(this.#db._raw());
    return rc;
  }

  /** Commit and free the handle, whether or not the commit succeeds. */
  commit() {
    const handle = this.#ensureOpen();
    const rc = fn.sekejap_tx_commit(handle);
    this.#handle = null;
    if (isNegOne(rc)) throw lastError(this.#db._raw());
  }

  /** Roll back and free the handle, whether or not the rollback succeeds. */
  rollback() {
    const handle = this.#ensureOpen();
    const rc = fn.sekejap_tx_rollback(handle);
    this.#handle = null;
    if (isNegOne(rc)) throw lastError(this.#db._raw());
  }
}

// ─────────────────────────────────────────────────────────────────────────
// 10. Db — the main handle
// ─────────────────────────────────────────────────────────────────────────

class Db {
  #handle;

  constructor(handle) {
    this.#handle = handle;
  }

  /** Internal: the raw handle, or null once closed. Used for error lookups. */
  _raw() {
    return this.#handle;
  }

  #ensureOpen() {
    if (!this.#handle) throw new Error('sekejap: database is closed');
    return this.#handle;
  }

  /** Open (or create) the database directory at `path`. */
  static open(path_) {
    const handle = fn.sekejap_open(path_);
    if (!handle) throw lastError(null);
    return new Db(handle);
  }

  /** `open`, under a store configuration: `{ budgetBytes, io, sync }`. */
  static openWithConfig(path_, config) {
    const json = config
      ? JSON.stringify({
          budget_bytes: config.budgetBytes,
          io: config.io,
          sync: config.sync,
        })
      : null;
    const handle = fn.sekejap_open_with_config(path_, json);
    if (!handle) throw lastError(null);
    return new Db(handle);
  }

  /** Open in SERVICE mode: one writer, parallel readers, the change feed. */
  static openService(path_) {
    const handle = fn.sekejap_open_service(path_);
    if (!handle) throw lastError(null);
    return new Db(handle);
  }

  /** REFUSED by name: sekejap has no in-memory store. Always throws. */
  static openMemory() {
    const handle = fn.sekejap_open_memory();
    if (!handle) throw lastError(null);
    return new Db(handle); // unreachable — always refused
  }

  /** Close and free. Uncommitted work is discarded: a close is not a commit. */
  close() {
    if (this.#handle) {
      fn.sekejap_close(this.#handle);
      this.#handle = null;
    }
  }

  // ── 4.3 Documents ───────────────────────────────────────────────────────

  /** Write one document, committed before this call returns. */
  put(collection, key, doc) {
    const body = { ...doc, _key: (doc && doc._key) ?? key };
    const rc = fn.sekejap_put(this.#ensureOpen(), collection, key, docJson(body));
    if (rc !== 0) throw lastError(this.#handle);
  }

  /** Write many documents into one collection under one commit. `rows`: `[{key, doc}]`. */
  putMany(collection, rows) {
    const rc = fn.sekejap_put_many(this.#ensureOpen(), collection, JSON.stringify(rows));
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc;
  }

  /** Read one document (with `_key` set), or `null` on a clean miss. */
  get(collection, key) {
    const handle = this.#ensureOpen();
    const json = fn.sekejap_get(handle, collection, key);
    if (json === null) {
      if (fn.sekejap_last_error_code(handle) !== SekejapStatus.Ok) throw lastError(handle);
      return null;
    }
    return JSON.parse(json);
  }

  /** Whether the row is there. */
  exists(collection, key) {
    const rc = fn.sekejap_exists(this.#ensureOpen(), collection, key);
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc === 1;
  }

  /** Delete one row and every edge that touches it, committed. */
  delete(collection, key) {
    const rc = fn.sekejap_delete(this.#ensureOpen(), collection, key);
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc === 1;
  }

  /** Open a walk of one collection in stable id order. `pageRows` of `0` means 256. */
  scan(collection, pageRows = 0) {
    const handle = fn.sekejap_scan_open(this.#ensureOpen(), collection, pageRows);
    if (!handle) throw lastError(this.#handle);
    return new Scan(handle, this, 'scan');
  }

  // ── 4.4 SQL ──────────────────────────────────────────────────────────────

  /** Run one writing statement, committed. Returns rows moved. */
  execute(sql, params) {
    const rc = fn.sekejap_execute(this.#ensureOpen(), sql, paramsJson(params));
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc;
  }

  /** Run one row-returning statement. Answer: array of row objects keyed by column. */
  query(sql, params) {
    const json = fn.sekejap_query(this.#ensureOpen(), sql, paramsJson(params));
    if (json === null) throw lastError(this.#handle);
    return JSON.parse(json);
  }

  /** The plan the engine would build for `sql`. */
  explain(sql, params) {
    const text = fn.sekejap_explain(this.#ensureOpen(), sql, paramsJson(params));
    if (text === null) throw lastError(this.#handle);
    return text;
  }

  /** Prepare one statement: parsed now, compiled by its first bind. */
  prepare(sql) {
    const handle = fn.sekejap_prepare(this.#ensureOpen(), sql);
    if (!handle) throw lastError(this.#handle);
    return new Statement(handle, this);
  }

  /** Run a row-returning statement and open a paged delivery of its answer. */
  stream(sql, params, pageRows = 0) {
    const handle = fn.sekejap_query_open(this.#ensureOpen(), sql, paramsJson(params), pageRows);
    if (!handle) throw lastError(this.#handle);
    return new Scan(handle, this, 'query');
  }

  // ── 4.5 Edges ────────────────────────────────────────────────────────────

  /** Link two rows with a typed edge in the base graph context, committed. */
  link(fromCollection, fromKey, edgeType, toCollection, toKey) {
    const rc = fn.sekejap_link(this.#ensureOpen(), fromCollection, fromKey, edgeType, toCollection, toKey);
    if (rc !== 0) throw lastError(this.#handle);
  }

  /** `link`, carrying a JSON properties object. */
  linkWith(fromCollection, fromKey, edgeType, toCollection, toKey, properties) {
    const rc = fn.sekejap_link_with(
      this.#ensureOpen(),
      fromCollection,
      fromKey,
      edgeType,
      toCollection,
      toKey,
      docJson(properties)
    );
    if (rc !== 0) throw lastError(this.#handle);
  }

  /** Remove one edge, committed. Returns whether it was there. */
  unlink(fromCollection, fromKey, edgeType, toCollection, toKey) {
    const rc = fn.sekejap_unlink(this.#ensureOpen(), fromCollection, fromKey, edgeType, toCollection, toKey);
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc === 1;
  }

  /**
   * The rows one hop away, in one direction. `edgeType` of `null`/`undefined`
   * means every type. `direction`: 'outgoing' | 'incoming' | 'both' (default),
   * or a raw SekejapDirection code. Bounded at 256 edges; wider needs SQL
   * GRAPH_TABLE.
   */
  neighbours(collection, key, edgeType, direction = 'both', limit = 256) {
    const json = fn.sekejap_neighbours(
      this.#ensureOpen(),
      collection,
      key,
      edgeType ?? null,
      directionCode(direction),
      limit
    );
    if (json === null) throw lastError(this.#handle);
    return JSON.parse(json);
  }

  // ── 4.6 The catalog ─────────────────────────────────────────────────────

  /** Declare a collection. `fields`: `[{name, kind, dimension?}]`. Returns whether it was newly created. */
  createCollection(name, fields) {
    const rc = fn.sekejap_create_collection(this.#ensureOpen(), name, JSON.stringify(fields ?? []));
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc === 1;
  }

  /** Remove a collection, its rows, its indexes and its descriptor. */
  dropCollection(name) {
    const rc = fn.sekejap_drop_collection(this.#ensureOpen(), name);
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc === 1;
  }

  /** Every collection name in the catalog, in key order. */
  collections() {
    const json = fn.sekejap_collections(this.#ensureOpen());
    if (json === null) throw lastError(this.#handle);
    return JSON.parse(json);
  }

  /** The declared shape of one collection, or `null` if there is no such collection. */
  describe(collection) {
    const handle = this.#ensureOpen();
    const json = fn.sekejap_describe(handle, collection);
    if (json === null) {
      if (fn.sekejap_last_error_code(handle) !== SekejapStatus.Ok) throw lastError(handle);
      return null;
    }
    return JSON.parse(json);
  }

  /** The rows of one collection: the live record when kept, else a walk. */
  countRows(collection) {
    const rc = fn.sekejap_count_rows(this.#ensureOpen(), collection);
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc;
  }

  /** Count the rows of one collection BY WALKING them. */
  scanCountRows(collection) {
    const rc = fn.sekejap_scan_count_rows(this.#ensureOpen(), collection);
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc;
  }

  /** Count every edge BY WALKING the primary edge keyspace. */
  scanCountEdges() {
    const rc = fn.sekejap_scan_count_edges(this.#ensureOpen());
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc;
  }

  // ── 4.7 Transactions ─────────────────────────────────────────────────────

  /** Take the writer for many writes under one barrier. */
  transaction() {
    const handle = fn.sekejap_tx_begin(this.#ensureOpen());
    if (!handle) throw lastError(this.#handle);
    return new Tx(handle, this);
  }

  // ── 4.8 Maintenance ──────────────────────────────────────────────────────

  /** Fold the committed WAL into the data file. `false` means deferred, not failed. */
  checkpoint() {
    const rc = fn.sekejap_checkpoint(this.#ensureOpen());
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc === 1;
  }

  /** Make the newest commit visible to readers now. */
  publish() {
    const rc = fn.sekejap_publish(this.#ensureOpen());
    if (isNegOne(rc)) throw lastError(this.#handle);
  }

  /** The bytes on disk: `{ dataBytes, walBytes, totalBytes }`. */
  storage() {
    const json = fn.sekejap_storage(this.#ensureOpen());
    if (json === null) throw lastError(this.#handle);
    const raw = JSON.parse(json);
    return { dataBytes: raw.data_bytes, walBytes: raw.wal_bytes, totalBytes: raw.total_bytes };
  }

  // ── 4.9 Service mode (REFUSED on a handle not opened with openService) ──

  /** Refuse a statement that runs longer than `milliseconds`. `0` clears it. */
  statementTimeoutMs(milliseconds) {
    const rc = fn.sekejap_statement_timeout_ms(this.#ensureOpen(), BigInt(milliseconds));
    if (isNegOne(rc)) throw lastError(this.#handle);
  }

  /** Cancel the work in flight, from any thread. Sticky until clearInterrupt(). */
  cancel() {
    const rc = fn.sekejap_cancel(this.#ensureOpen());
    if (isNegOne(rc)) throw lastError(this.#handle);
  }

  /** Clear a cancel. Returns whether one was standing. */
  clearInterrupt() {
    const rc = fn.sekejap_clear_interrupt(this.#ensureOpen());
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc === 1;
  }

  /** Subscribe to the commit-time change feed. Returns a subscription id. */
  subscribe() {
    const rc = fn.sekejap_subscribe(this.#ensureOpen());
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc;
  }

  /** The next change event for one subscription, or `null` when none arrived. */
  nextChange(subscriptionId, timeoutMs = 0) {
    const handle = this.#ensureOpen();
    const json = fn.sekejap_next_change(handle, subscriptionId, BigInt(timeoutMs));
    if (json === null) {
      if (fn.sekejap_last_error_code(handle) !== SekejapStatus.Ok) throw lastError(handle);
      return null;
    }
    return JSON.parse(json);
  }

  /** Close one subscription. Returns whether it was open. */
  unsubscribe(subscriptionId) {
    const rc = fn.sekejap_unsubscribe(this.#ensureOpen(), subscriptionId);
    if (isNegOne(rc)) throw lastError(this.#handle);
    return rc === 1;
  }

  // ── 4.10 Refused by name ─────────────────────────────────────────────────

  /** REFUSED: no proportional-to-rows memory to trim. Always throws. */
  trimMemory() {
    const rc = fn.sekejap_trim_memory(this.#ensureOpen());
    if (isNegOne(rc)) throw lastError(this.#handle);
  }

  /** REFUSED: no payload-rewriting compaction. Always throws; use checkpoint(). */
  compact() {
    const rc = fn.sekejap_compact(this.#ensureOpen());
    if (isNegOne(rc)) throw lastError(this.#handle);
  }

  /** REFUSED: SHOW has no Tier-1 spelling. Always throws; use collections()/describe(). */
  show(statement) {
    const text = fn.sekejap_show(this.#ensureOpen(), statement ?? null);
    if (text === null) throw lastError(this.#handle);
    return text;
  }
}

// ─────────────────────────────────────────────────────────────────────────
// 11. Module-level, no-handle calls
// ─────────────────────────────────────────────────────────────────────────

/** The library version, as `MAJOR.MINOR.PATCH`. */
function version() {
  return fn.sekejap_version();
}

/** The sekejap disk format this build reads and writes. */
function formatVersion() {
  return fn.sekejap_format_version();
}

module.exports = {
  Db,
  Statement,
  Scan,
  Tx,
  SekejapError,
  SekejapStatus,
  SekejapDirection,
  version,
  formatVersion,
};
