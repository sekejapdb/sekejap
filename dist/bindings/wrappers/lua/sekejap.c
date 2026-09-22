// sekejap.c — Lua 5.4 C-module binding for the sekejap 0.17 C ABI
// (docs/dist/C_ABI.md), over dist/ffi/include/sekejap.h.
//
// `require("sekejap")` returns a module table (`open`, `open_with_config`,
// `open_service`, `open_memory`, `version`, `format_version`, plus the nine
// `STATUS_*` constants of `SekejapStatus`). An open database is a userdata
// (metatable "sekejap.db") with one method per C function that takes a
// `SekejapDb*`; `db:prepare`, `db:scan_open`, `db:query_open` and
// `db:tx_begin` return their own userdata ("sekejap.stmt", "sekejap.scan",
// "sekejap.tx") with the matching methods. A Stmt/Scan/Tx keeps a reference
// (its first uservalue) to the Lua Db object it was taken from, so the Db
// is not garbage-collected out from under it -- but the C ABI still
// requires every Stmt/Scan/Tx to be closed BEFORE the Db that made it, and
// this wrapper does not reorder that beyond what the library itself does.
//
// Handles free themselves on garbage collection (__gc, and __close for Lua
// 5.4 to-be-closed variables); `db:close()` / `stmt:free()` /
// `scan:close()` / `tx:commit()` / `tx:rollback()` are the explicit, early
// form of the same free and are idempotent. Hard failures raise Lua errors
// (pcall-catchable) carrying sekejap_last_error(); `db:last_error()` and
// `db:last_error_code()` are also exposed directly for a caller that wants
// to inspect rather than catch. Documents, parameters and rows cross as
// JSON TEXT (this module does no JSON decoding), matching the C ABI.
//
// Build: see the Makefile (loadable module linking the prebuilt
// libsekejap; no Rust is built from this directory).
#include <string.h>

#include <lauxlib.h>
#include <lua.h>

#include "sekejap.h"

#define DB_MT "sekejap.db"
#define STMT_MT "sekejap.stmt"
#define SCAN_MT "sekejap.scan"
#define TX_MT "sekejap.tx"

typedef struct { SekejapDb *db; } LDb;
typedef struct { SekejapStmt *stmt; } LStmt;
typedef struct { SekejapScan *scan; } LScan;
typedef struct { SekejapTx *tx; } LTx;

// ── small helpers ───────────────────────────────────────────────────────────

static LDb *check_db(lua_State *L, int i) {
    LDb *d = (LDb *)luaL_checkudata(L, i, DB_MT);
    if (d->db == NULL) luaL_error(L, "sekejap: database is closed");
    return d;
}

static LStmt *check_stmt(lua_State *L, int i) {
    LStmt *s = (LStmt *)luaL_checkudata(L, i, STMT_MT);
    if (s->stmt == NULL) luaL_error(L, "sekejap: statement already freed");
    return s;
}

static LScan *check_scan(lua_State *L, int i) {
    LScan *s = (LScan *)luaL_checkudata(L, i, SCAN_MT);
    if (s->scan == NULL) luaL_error(L, "sekejap: scan already closed");
    return s;
}

static LTx *check_tx(lua_State *L, int i) {
    LTx *t = (LTx *)luaL_checkudata(L, i, TX_MT);
    if (t->tx == NULL) luaL_error(L, "sekejap: transaction already committed or rolled back");
    return t;
}

// nil/absent -> NULL (the C ABI's "no parameters" / "default config"), else
// the C string.
static const char *opt_str(lua_State *L, int idx) {
    if (lua_isnoneornil(L, idx)) return NULL;
    return luaL_checkstring(L, idx);
}

static uintptr_t opt_uint(lua_State *L, int idx, uintptr_t def) {
    if (lua_isnoneornil(L, idx)) return def;
    lua_Integer v = luaL_checkinteger(L, idx);
    luaL_argcheck(L, v >= 0, idx, "must not be negative");
    return (uintptr_t)v;
}

static SekejapDirection check_direction(lua_State *L, int idx) {
    if (lua_isnoneornil(L, idx)) return SekejapDirection_Outgoing;
    const char *s = luaL_checkstring(L, idx);
    if (strcmp(s, "outgoing") == 0) return SekejapDirection_Outgoing;
    if (strcmp(s, "incoming") == 0) return SekejapDirection_Incoming;
    if (strcmp(s, "both") == 0) return SekejapDirection_Both;
    return luaL_error(L, "sekejap: direction must be 'outgoing', 'incoming' or 'both' (got '%s')", s);
}

// Push an owned C string as a Lua string, then free it (the C ABI's "free"
// ownership column). NULL pushes nil.
static int push_owned_or_nil(lua_State *L, char *s) {
    if (s == NULL) { lua_pushnil(L); return 1; }
    lua_pushstring(L, s);
    sekejap_string_free(s);
    return 1;
}

static int push_bool(lua_State *L, int32_t flag) {
    lua_pushboolean(L, flag != 0);
    return 1;
}

// Raise a Lua error carrying sekejap_last_error(). `db` is accepted for
// clarity at call sites but ignored by the library (the slot is
// thread-local), so NULL is always safe to pass.
static int raise_last(lua_State *L, SekejapDb *db, const char *what) {
    char *e = sekejap_last_error(db);
    if (e != NULL) {
        lua_pushfstring(L, "sekejap %s: %s", what, e);
        sekejap_string_free(e);
        return lua_error(L);
    }
    return luaL_error(L, "sekejap %s failed", what);
}

// Keep the owning Db userdata (at stack index `db_idx`) alive as long as
// the just-created derived handle (at the top of the stack) is.
static void keep_alive(lua_State *L, int db_idx) {
    lua_pushvalue(L, db_idx);
    lua_setiuservalue(L, -2, 1);
}

// ── Db: opening and identity ────────────────────────────────────────────────

static int l_open(lua_State *L) {
    const char *path = luaL_checkstring(L, 1);
    SekejapDb *db = sekejap_open(path);
    if (db == NULL) return raise_last(L, NULL, "open");
    LDb *d = (LDb *)lua_newuserdatauv(L, sizeof(LDb), 0);
    d->db = db;
    luaL_setmetatable(L, DB_MT);
    return 1;
}

static int l_open_with_config(lua_State *L) {
    const char *path = luaL_checkstring(L, 1);
    SekejapDb *db = sekejap_open_with_config(path, opt_str(L, 2));
    if (db == NULL) return raise_last(L, NULL, "open_with_config");
    LDb *d = (LDb *)lua_newuserdatauv(L, sizeof(LDb), 0);
    d->db = db;
    luaL_setmetatable(L, DB_MT);
    return 1;
}

static int l_open_service(lua_State *L) {
    const char *path = luaL_checkstring(L, 1);
    SekejapDb *db = sekejap_open_service(path);
    if (db == NULL) return raise_last(L, NULL, "open_service");
    LDb *d = (LDb *)lua_newuserdatauv(L, sizeof(LDb), 0);
    d->db = db;
    luaL_setmetatable(L, DB_MT);
    return 1;
}

// REFUSED by the library: sekejap is disk-first and has no in-memory store.
static int l_open_memory(lua_State *L) {
    SekejapDb *db = sekejap_open_memory();
    if (db == NULL) return raise_last(L, NULL, "open_memory");
    LDb *d = (LDb *)lua_newuserdatauv(L, sizeof(LDb), 0);
    d->db = db;
    luaL_setmetatable(L, DB_MT);
    return 1;
}

static int l_version(lua_State *L) {
    lua_pushstring(L, sekejap_version()); // static: never freed
    return 1;
}

static int l_format_version(lua_State *L) {
    lua_pushinteger(L, sekejap_format_version());
    return 1;
}

static int l_close(lua_State *L) {
    LDb *d = (LDb *)luaL_checkudata(L, 1, DB_MT);
    if (d->db != NULL) {
        sekejap_close(d->db);
        d->db = NULL;
    }
    return 0;
}

// ── Db: errors and memory ───────────────────────────────────────────────────

static int l_last_error(lua_State *L) {
    LDb *d = check_db(L, 1);
    return push_owned_or_nil(L, sekejap_last_error(d->db));
}

static int l_last_error_code(lua_State *L) {
    LDb *d = check_db(L, 1);
    lua_pushinteger(L, sekejap_last_error_code(d->db));
    return 1;
}

// ── Db: documents ────────────────────────────────────────────────────────────

static int l_put(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    const char *key = luaL_checkstring(L, 3);
    const char *doc = luaL_checkstring(L, 4);
    if (sekejap_put(d->db, collection, key, doc) < 0) return raise_last(L, d->db, "put");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_put_many(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    const char *rows_json = luaL_checkstring(L, 3);
    long n = sekejap_put_many(d->db, collection, rows_json);
    if (n < 0) return raise_last(L, d->db, "put_many");
    lua_pushinteger(L, n);
    return 1;
}

// nil = clean miss; a raised error = a real failure (the two are told apart
// by whether an error slot was set, per C_ABI.md §1).
static int l_get(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    const char *key = luaL_checkstring(L, 3);
    char *r = sekejap_get(d->db, collection, key);
    if (r != NULL) return push_owned_or_nil(L, r);
    if (sekejap_last_error_code(d->db) == SekejapStatus_Ok) { lua_pushnil(L); return 1; }
    return raise_last(L, d->db, "get");
}

static int l_exists(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    const char *key = luaL_checkstring(L, 3);
    int32_t r = sekejap_exists(d->db, collection, key);
    if (r < 0) return raise_last(L, d->db, "exists");
    return push_bool(L, r);
}

static int l_delete(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    const char *key = luaL_checkstring(L, 3);
    int32_t r = sekejap_delete(d->db, collection, key);
    if (r < 0) return raise_last(L, d->db, "delete");
    return push_bool(L, r);
}

static int l_scan_open(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    uintptr_t page_rows = opt_uint(L, 3, 0);
    SekejapScan *sc = sekejap_scan_open(d->db, collection, page_rows);
    if (sc == NULL) return raise_last(L, d->db, "scan_open");
    LScan *ls = (LScan *)lua_newuserdatauv(L, sizeof(LScan), 1);
    ls->scan = sc;
    luaL_setmetatable(L, SCAN_MT);
    keep_alive(L, 1);
    return 1;
}

// ── Db: SQL ──────────────────────────────────────────────────────────────────

static int l_execute(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *sql = luaL_checkstring(L, 2);
    const char *params = opt_str(L, 3);
    long n = sekejap_execute(d->db, sql, params);
    if (n < 0) return raise_last(L, d->db, "execute");
    lua_pushinteger(L, n);
    return 1;
}

static int l_query(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *sql = luaL_checkstring(L, 2);
    const char *params = opt_str(L, 3);
    char *r = sekejap_query(d->db, sql, params);
    if (r == NULL) return raise_last(L, d->db, "query");
    return push_owned_or_nil(L, r);
}

static int l_explain(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *sql = luaL_checkstring(L, 2);
    const char *params = opt_str(L, 3);
    char *r = sekejap_explain(d->db, sql, params);
    if (r == NULL) return raise_last(L, d->db, "explain");
    return push_owned_or_nil(L, r);
}

static int l_prepare(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *sql = luaL_checkstring(L, 2);
    SekejapStmt *s = sekejap_prepare(d->db, sql);
    if (s == NULL) return raise_last(L, d->db, "prepare");
    LStmt *ls = (LStmt *)lua_newuserdatauv(L, sizeof(LStmt), 1);
    ls->stmt = s;
    luaL_setmetatable(L, STMT_MT);
    keep_alive(L, 1);
    return 1;
}

static int l_query_open(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *sql = luaL_checkstring(L, 2);
    const char *params = opt_str(L, 3);
    uintptr_t page_rows = opt_uint(L, 4, 0);
    SekejapScan *sc = sekejap_query_open(d->db, sql, params, page_rows);
    if (sc == NULL) return raise_last(L, d->db, "query_open");
    LScan *ls = (LScan *)lua_newuserdatauv(L, sizeof(LScan), 1);
    ls->scan = sc;
    luaL_setmetatable(L, SCAN_MT);
    keep_alive(L, 1);
    return 1;
}

// ── Db: edges ────────────────────────────────────────────────────────────────

static int l_link(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *fc = luaL_checkstring(L, 2), *fk = luaL_checkstring(L, 3);
    const char *et = luaL_checkstring(L, 4);
    const char *tc = luaL_checkstring(L, 5), *tk = luaL_checkstring(L, 6);
    if (sekejap_link(d->db, fc, fk, et, tc, tk) < 0) return raise_last(L, d->db, "link");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_link_with(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *fc = luaL_checkstring(L, 2), *fk = luaL_checkstring(L, 3);
    const char *et = luaL_checkstring(L, 4);
    const char *tc = luaL_checkstring(L, 5), *tk = luaL_checkstring(L, 6);
    const char *props = luaL_checkstring(L, 7);
    if (sekejap_link_with(d->db, fc, fk, et, tc, tk, props) < 0) return raise_last(L, d->db, "link_with");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_unlink(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *fc = luaL_checkstring(L, 2), *fk = luaL_checkstring(L, 3);
    const char *et = luaL_checkstring(L, 4);
    const char *tc = luaL_checkstring(L, 5), *tk = luaL_checkstring(L, 6);
    int32_t r = sekejap_unlink(d->db, fc, fk, et, tc, tk);
    if (r < 0) return raise_last(L, d->db, "unlink");
    return push_bool(L, r);
}

static int l_neighbours(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    const char *key = luaL_checkstring(L, 3);
    const char *edge_type = opt_str(L, 4);
    SekejapDirection dir = check_direction(L, 5);
    uintptr_t limit = opt_uint(L, 6, 256);
    char *r = sekejap_neighbours(d->db, collection, key, edge_type, dir, limit);
    if (r == NULL) return raise_last(L, d->db, "neighbours");
    return push_owned_or_nil(L, r);
}

// ── Db: catalog ──────────────────────────────────────────────────────────────

static int l_create_collection(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *name = luaL_checkstring(L, 2);
    const char *fields = lua_isnoneornil(L, 3) ? "[]" : luaL_checkstring(L, 3);
    int32_t r = sekejap_create_collection(d->db, name, fields);
    if (r < 0) return raise_last(L, d->db, "create_collection");
    return push_bool(L, r); // true: created now, false: already in the catalog
}

static int l_drop_collection(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *name = luaL_checkstring(L, 2);
    int32_t r = sekejap_drop_collection(d->db, name);
    if (r < 0) return raise_last(L, d->db, "drop_collection");
    return push_bool(L, r);
}

static int l_collections(lua_State *L) {
    LDb *d = check_db(L, 1);
    char *r = sekejap_collections(d->db);
    if (r == NULL) return raise_last(L, d->db, "collections");
    return push_owned_or_nil(L, r);
}

static int l_describe(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    char *r = sekejap_describe(d->db, collection);
    if (r != NULL) return push_owned_or_nil(L, r);
    if (sekejap_last_error_code(d->db) == SekejapStatus_Ok) { lua_pushnil(L); return 1; }
    return raise_last(L, d->db, "describe");
}

static int l_count_rows(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    long n = sekejap_count_rows(d->db, collection);
    if (n < 0) return raise_last(L, d->db, "count_rows");
    lua_pushinteger(L, n);
    return 1;
}

static int l_scan_count_rows(lua_State *L) {
    LDb *d = check_db(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    long n = sekejap_scan_count_rows(d->db, collection);
    if (n < 0) return raise_last(L, d->db, "scan_count_rows");
    lua_pushinteger(L, n);
    return 1;
}

static int l_scan_count_edges(lua_State *L) {
    LDb *d = check_db(L, 1);
    long n = sekejap_scan_count_edges(d->db);
    if (n < 0) return raise_last(L, d->db, "scan_count_edges");
    lua_pushinteger(L, n);
    return 1;
}

// ── Db: transactions ─────────────────────────────────────────────────────────

static int l_tx_begin(lua_State *L) {
    LDb *d = check_db(L, 1);
    SekejapTx *tx = sekejap_tx_begin(d->db);
    if (tx == NULL) return raise_last(L, d->db, "tx_begin");
    LTx *lt = (LTx *)lua_newuserdatauv(L, sizeof(LTx), 1);
    lt->tx = tx;
    luaL_setmetatable(L, TX_MT);
    keep_alive(L, 1);
    return 1;
}

// ── Db: maintenance ──────────────────────────────────────────────────────────

static int l_checkpoint(lua_State *L) {
    LDb *d = check_db(L, 1);
    int32_t r = sekejap_checkpoint(d->db);
    if (r < 0) return raise_last(L, d->db, "checkpoint");
    return push_bool(L, r); // true: folded, false: deferred (a live reader holds a slot)
}

static int l_publish(lua_State *L) {
    LDb *d = check_db(L, 1);
    if (sekejap_publish(d->db) < 0) return raise_last(L, d->db, "publish");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_storage(lua_State *L) {
    LDb *d = check_db(L, 1);
    char *r = sekejap_storage(d->db);
    if (r == NULL) return raise_last(L, d->db, "storage");
    return push_owned_or_nil(L, r);
}

static int l_trim_memory(lua_State *L) { // REFUSED: nothing proportional to rows to trim
    LDb *d = check_db(L, 1);
    if (sekejap_trim_memory(d->db) < 0) return raise_last(L, d->db, "trim_memory");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_compact(lua_State *L) { // REFUSED: no payload-rewriting compaction
    LDb *d = check_db(L, 1);
    if (sekejap_compact(d->db) < 0) return raise_last(L, d->db, "compact");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_show(lua_State *L) { // REFUSED: no SHOW dialect (use collections()/describe())
    LDb *d = check_db(L, 1);
    const char *stmt = opt_str(L, 2);
    char *r = sekejap_show(d->db, stmt);
    if (r != NULL) return push_owned_or_nil(L, r);
    return raise_last(L, d->db, "show");
}

// ── Db: service mode ─────────────────────────────────────────────────────────

static int l_statement_timeout_ms(lua_State *L) {
    LDb *d = check_db(L, 1);
    lua_Integer ms = luaL_checkinteger(L, 2);
    luaL_argcheck(L, ms >= 0, 2, "must not be negative");
    if (sekejap_statement_timeout_ms(d->db, (uint64_t)ms) < 0) return raise_last(L, d->db, "statement_timeout_ms");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_cancel(lua_State *L) {
    LDb *d = check_db(L, 1);
    if (sekejap_cancel(d->db) < 0) return raise_last(L, d->db, "cancel");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_clear_interrupt(lua_State *L) {
    LDb *d = check_db(L, 1);
    int32_t r = sekejap_clear_interrupt(d->db);
    if (r < 0) return raise_last(L, d->db, "clear_interrupt");
    return push_bool(L, r);
}

static int l_subscribe(lua_State *L) {
    LDb *d = check_db(L, 1);
    long id = sekejap_subscribe(d->db);
    if (id < 0) return raise_last(L, d->db, "subscribe");
    lua_pushinteger(L, id);
    return 1;
}

static int l_next_change(lua_State *L) {
    LDb *d = check_db(L, 1);
    lua_Integer sub = luaL_checkinteger(L, 2);
    lua_Integer timeout_ms = luaL_optinteger(L, 3, 0);
    luaL_argcheck(L, timeout_ms >= 0, 3, "must not be negative");
    char *r = sekejap_next_change(d->db, (long)sub, (uint64_t)timeout_ms);
    if (r != NULL) return push_owned_or_nil(L, r);
    if (sekejap_last_error_code(d->db) == SekejapStatus_Ok) { lua_pushnil(L); return 1; } // none arrived
    return raise_last(L, d->db, "next_change");
}

static int l_unsubscribe(lua_State *L) {
    LDb *d = check_db(L, 1);
    lua_Integer sub = luaL_checkinteger(L, 2);
    int32_t r = sekejap_unsubscribe(d->db, (long)sub);
    if (r < 0) return raise_last(L, d->db, "unsubscribe");
    return push_bool(L, r);
}

// ── Stmt ─────────────────────────────────────────────────────────────────────

static int l_stmt_query(lua_State *L) {
    LStmt *s = check_stmt(L, 1);
    const char *params = opt_str(L, 2);
    char *r = sekejap_stmt_query(s->stmt, params);
    if (r == NULL) return raise_last(L, NULL, "stmt:query");
    return push_owned_or_nil(L, r);
}

static int l_stmt_execute(lua_State *L) {
    LStmt *s = check_stmt(L, 1);
    const char *params = opt_str(L, 2);
    long n = sekejap_stmt_execute(s->stmt, params);
    if (n < 0) return raise_last(L, NULL, "stmt:execute");
    lua_pushinteger(L, n);
    return 1;
}

// true: a further bind compiles nothing; false: it does; nil: not bound yet.
static int l_stmt_rebindable(lua_State *L) {
    LStmt *s = check_stmt(L, 1);
    int32_t r = sekejap_stmt_rebindable(s->stmt);
    if (r < 0) return raise_last(L, NULL, "stmt:rebindable");
    if (r == SEKEJAP_REBIND_UNBOUND) { lua_pushnil(L); return 1; }
    return push_bool(L, r);
}

static int l_stmt_free(lua_State *L) {
    LStmt *s = (LStmt *)luaL_checkudata(L, 1, STMT_MT);
    if (s->stmt != NULL) { sekejap_stmt_free(s->stmt); s->stmt = NULL; }
    return 0;
}

// ── Scan (also backs query_open's paged answer: the ABI names it the same
// operation) ─────────────────────────────────────────────────────────────────

// The next page, or nil at a clean end of the walk (told apart from a real
// failure the same way as db:get(): a NULL with SekejapStatus_Ok is the
// end, not an error).
static int l_scan_next(lua_State *L) {
    LScan *s = check_scan(L, 1);
    char *r = sekejap_scan_next(s->scan);
    if (r != NULL) return push_owned_or_nil(L, r);
    if (sekejap_last_error_code(NULL) == SekejapStatus_Ok) { lua_pushnil(L); return 1; }
    return raise_last(L, NULL, "scan:next");
}

static int l_scan_close(lua_State *L) {
    LScan *s = (LScan *)luaL_checkudata(L, 1, SCAN_MT);
    if (s->scan != NULL) { sekejap_scan_close(s->scan); s->scan = NULL; }
    return 0;
}

// ── Tx ───────────────────────────────────────────────────────────────────────

static int l_tx_put(lua_State *L) {
    LTx *t = check_tx(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    const char *key = luaL_checkstring(L, 3);
    const char *doc = luaL_checkstring(L, 4);
    if (sekejap_tx_put(t->tx, collection, key, doc) < 0) return raise_last(L, NULL, "tx:put");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_tx_delete(lua_State *L) {
    LTx *t = check_tx(L, 1);
    const char *collection = luaL_checkstring(L, 2);
    const char *key = luaL_checkstring(L, 3);
    int32_t r = sekejap_tx_delete(t->tx, collection, key);
    if (r < 0) return raise_last(L, NULL, "tx:delete");
    return push_bool(L, r);
}

static int l_tx_link(lua_State *L) {
    LTx *t = check_tx(L, 1);
    const char *fc = luaL_checkstring(L, 2), *fk = luaL_checkstring(L, 3);
    const char *et = luaL_checkstring(L, 4);
    const char *tc = luaL_checkstring(L, 5), *tk = luaL_checkstring(L, 6);
    if (sekejap_tx_link(t->tx, fc, fk, et, tc, tk) < 0) return raise_last(L, NULL, "tx:link");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_tx_execute(lua_State *L) {
    LTx *t = check_tx(L, 1);
    const char *sql = luaL_checkstring(L, 2);
    const char *params = opt_str(L, 3);
    long n = sekejap_tx_execute(t->tx, sql, params);
    if (n < 0) return raise_last(L, NULL, "tx:execute");
    lua_pushinteger(L, n);
    return 1;
}

// Commit/rollback free the handle whether they succeed or not (the C ABI's
// rule), so the Lua side always marks it dead too.
static int l_tx_commit(lua_State *L) {
    LTx *t = (LTx *)luaL_checkudata(L, 1, TX_MT);
    if (t->tx == NULL) luaL_error(L, "sekejap: transaction already committed or rolled back");
    SekejapTx *tx = t->tx;
    t->tx = NULL;
    if (sekejap_tx_commit(tx) < 0) return raise_last(L, NULL, "tx:commit");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_tx_rollback(lua_State *L) {
    LTx *t = (LTx *)luaL_checkudata(L, 1, TX_MT);
    if (t->tx == NULL) luaL_error(L, "sekejap: transaction already committed or rolled back");
    SekejapTx *tx = t->tx;
    t->tx = NULL;
    if (sekejap_tx_rollback(tx) < 0) return raise_last(L, NULL, "tx:rollback");
    lua_pushboolean(L, 1);
    return 1;
}

// A handle dropped any other way (GC without an explicit commit/rollback)
// rolls back, per the C ABI.
static int l_tx_gc(lua_State *L) {
    LTx *t = (LTx *)luaL_checkudata(L, 1, TX_MT);
    if (t->tx != NULL) { sekejap_tx_rollback(t->tx); t->tx = NULL; }
    return 0;
}

// ── registration ─────────────────────────────────────────────────────────────

static const luaL_Reg db_methods[] = {
    {"close", l_close},
    {"last_error", l_last_error},
    {"last_error_code", l_last_error_code},
    {"put", l_put},
    {"put_many", l_put_many},
    {"get", l_get},
    {"exists", l_exists},
    {"delete", l_delete},
    {"scan_open", l_scan_open},
    {"execute", l_execute},
    {"query", l_query},
    {"explain", l_explain},
    {"prepare", l_prepare},
    {"query_open", l_query_open},
    {"link", l_link},
    {"link_with", l_link_with},
    {"unlink", l_unlink},
    {"neighbours", l_neighbours},
    {"create_collection", l_create_collection},
    {"drop_collection", l_drop_collection},
    {"collections", l_collections},
    {"describe", l_describe},
    {"count_rows", l_count_rows},
    {"scan_count_rows", l_scan_count_rows},
    {"scan_count_edges", l_scan_count_edges},
    {"tx_begin", l_tx_begin},
    {"checkpoint", l_checkpoint},
    {"publish", l_publish},
    {"storage", l_storage},
    {"trim_memory", l_trim_memory},
    {"compact", l_compact},
    {"show", l_show},
    {"statement_timeout_ms", l_statement_timeout_ms},
    {"cancel", l_cancel},
    {"clear_interrupt", l_clear_interrupt},
    {"subscribe", l_subscribe},
    {"next_change", l_next_change},
    {"unsubscribe", l_unsubscribe},
    {NULL, NULL},
};

static const luaL_Reg stmt_methods[] = {
    {"query", l_stmt_query},
    {"execute", l_stmt_execute},
    {"rebindable", l_stmt_rebindable},
    {"free", l_stmt_free},
    {NULL, NULL},
};

static const luaL_Reg scan_methods[] = {
    {"next", l_scan_next},
    {"close", l_scan_close},
    {NULL, NULL},
};

static const luaL_Reg tx_methods[] = {
    {"put", l_tx_put},
    {"delete", l_tx_delete},
    {"link", l_tx_link},
    {"execute", l_tx_execute},
    {"commit", l_tx_commit},
    {"rollback", l_tx_rollback},
    {NULL, NULL},
};

static const luaL_Reg module_fns[] = {
    {"open", l_open},
    {"open_with_config", l_open_with_config},
    {"open_service", l_open_service},
    {"open_memory", l_open_memory},
    {"version", l_version},
    {"format_version", l_format_version},
    {NULL, NULL},
};

// __index-as-table metatable, with self-closing on __gc (and __close for
// Lua 5.4 to-be-closed variables).
static void make_method_metatable(lua_State *L, const char *name, const luaL_Reg *methods,
                                   lua_CFunction gc) {
    luaL_newmetatable(L, name);
    lua_pushcfunction(L, gc);
    lua_setfield(L, -2, "__gc");
    lua_pushcfunction(L, gc);
    lua_setfield(L, -2, "__close");
    lua_newtable(L);
    luaL_setfuncs(L, methods, 0);
    lua_setfield(L, -2, "__index");
    lua_pop(L, 1);
}

int luaopen_sekejap(lua_State *L) {
    make_method_metatable(L, DB_MT, db_methods, l_close);
    make_method_metatable(L, STMT_MT, stmt_methods, l_stmt_free);
    make_method_metatable(L, SCAN_MT, scan_methods, l_scan_close);
    make_method_metatable(L, TX_MT, tx_methods, l_tx_gc);

    luaL_newlib(L, module_fns);

    lua_pushinteger(L, SekejapStatus_Ok); lua_setfield(L, -2, "STATUS_OK");
    lua_pushinteger(L, SekejapStatus_Refused); lua_setfield(L, -2, "STATUS_REFUSED");
    lua_pushinteger(L, SekejapStatus_Corrupt); lua_setfield(L, -2, "STATUS_CORRUPT");
    lua_pushinteger(L, SekejapStatus_Unsupported); lua_setfield(L, -2, "STATUS_UNSUPPORTED");
    lua_pushinteger(L, SekejapStatus_Io); lua_setfield(L, -2, "STATUS_IO");
    lua_pushinteger(L, SekejapStatus_Invalid); lua_setfield(L, -2, "STATUS_INVALID");
    lua_pushinteger(L, SekejapStatus_Busy); lua_setfield(L, -2, "STATUS_BUSY");
    lua_pushinteger(L, SekejapStatus_UnknownRow); lua_setfield(L, -2, "STATUS_UNKNOWN_ROW");
    lua_pushinteger(L, SekejapStatus_Unknown); lua_setfield(L, -2, "STATUS_UNKNOWN");

    return 1;
}
