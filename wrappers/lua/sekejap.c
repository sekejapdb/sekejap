// sekejap.c — Lua 5.4 C-module binding for the sekejap C ABI.
//
// A tiny, table-based API: `require("sekejap")` returns a module with `open`
// and `version`; an open database is a userdata with methods (`db:execute(...)`,
// `db:query(...)`, …). Handles close themselves on garbage-collection; hard
// failures raise Lua errors (pcall-catchable) carrying sekejap_last_error().
//
// Build: see the Makefile (loadable module linking libsekejap).
#include <lauxlib.h>
#include <lua.h>

#include "sekejap.h"

#define DB_MT "sekejap.db"
#define STMT_MT "sekejap.stmt"

typedef struct {
    SekejapDb *db;
} LDb;

typedef struct {
    SekejapStmt *stmt;
} LStmt;

static LDb *check_db(lua_State *L, int i) {
    LDb *d = (LDb *)luaL_checkudata(L, i, DB_MT);
    if (d->db == NULL) luaL_error(L, "sekejap: database is closed");
    return d;
}

// Push an owned C string, then free it (per the C ABI contract).
static int push_owned(lua_State *L, char *s) {
    if (s == NULL) {
        lua_pushnil(L);
        return 1;
    }
    lua_pushstring(L, s);
    sekejap_string_free(s);
    return 1;
}

// Raise a Lua error carrying sekejap_last_error(), without leaking it.
static int raise_last(lua_State *L, SekejapDb *db, const char *what) {
    char *e = sekejap_last_error(db);
    if (e != NULL) {
        lua_pushfstring(L, "sekejap %s: %s", what, e); // Lua copies the message
        sekejap_string_free(e);
        return lua_error(L);
    }
    return luaL_error(L, "sekejap %s failed", what);
}

// ── methods ─────────────────────────────────────────────────────────────────

static int l_execute(lua_State *L) {
    LDb *d = check_db(L, 1);
    long n = sekejap_execute(d->db, luaL_checkstring(L, 2));
    if (n < 0) return raise_last(L, d->db, "execute");
    lua_pushinteger(L, n);
    return 1;
}

static int l_execute_params(lua_State *L) {
    LDb *d = check_db(L, 1);
    long n = sekejap_execute_params(d->db, luaL_checkstring(L, 2), luaL_checkstring(L, 3));
    if (n < 0) return raise_last(L, d->db, "execute_params");
    lua_pushinteger(L, n);
    return 1;
}

static int l_query(lua_State *L) {
    LDb *d = check_db(L, 1);
    char *r = sekejap_query(d->db, luaL_checkstring(L, 2));
    if (r == NULL) return raise_last(L, d->db, "query");
    return push_owned(L, r);
}

static int l_query_params(lua_State *L) {
    LDb *d = check_db(L, 1);
    char *r = sekejap_query_params(d->db, luaL_checkstring(L, 2), luaL_checkstring(L, 3));
    if (r == NULL) return raise_last(L, d->db, "query_params");
    return push_owned(L, r);
}

static int l_prepare(lua_State *L) {
    LDb *d = check_db(L, 1);
    SekejapStmt *s = sekejap_prepare(d->db, luaL_checkstring(L, 2));
    if (s == NULL) return raise_last(L, d->db, "prepare");
    LStmt *ls = (LStmt *)lua_newuserdatauv(L, sizeof(LStmt), 0);
    ls->stmt = s;
    luaL_setmetatable(L, STMT_MT);
    return 1;
}

static int l_query_prepared(lua_State *L) {
    LDb *d = check_db(L, 1);
    LStmt *ls = (LStmt *)luaL_checkudata(L, 2, STMT_MT);
    if (ls->stmt == NULL) luaL_error(L, "sekejap: statement already freed");
    char *r = sekejap_query_prepared(d->db, ls->stmt, luaL_checkstring(L, 3));
    if (r == NULL) return raise_last(L, d->db, "query_prepared");
    return push_owned(L, r);
}

static int l_put(lua_State *L) {
    LDb *d = check_db(L, 1);
    if (sekejap_put(d->db, luaL_checkstring(L, 2), luaL_checkstring(L, 3)) < 0)
        return raise_last(L, d->db, "put");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_put_many(lua_State *L) {
    LDb *d = check_db(L, 1);
    long n = sekejap_put_many(d->db, luaL_checkstring(L, 2));
    if (n < 0) return raise_last(L, d->db, "put_many");
    lua_pushinteger(L, n);
    return 1;
}

static int l_get(lua_State *L) {
    LDb *d = check_db(L, 1);
    char *r = sekejap_get(d->db, luaL_checkstring(L, 2));
    if (r != NULL) return push_owned(L, r);
    char *e = sekejap_last_error(d->db); // null: distinguish clean miss from error
    if (e != NULL) {
        lua_pushfstring(L, "sekejap get: %s", e);
        sekejap_string_free(e);
        return lua_error(L);
    }
    lua_pushnil(L); // clean miss
    return 1;
}

static int l_remove(lua_State *L) {
    LDb *d = check_db(L, 1);
    if (sekejap_remove(d->db, luaL_checkstring(L, 2)) < 0) return raise_last(L, d->db, "remove");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_link(lua_State *L) {
    LDb *d = check_db(L, 1);
    if (sekejap_link(d->db, luaL_checkstring(L, 2), luaL_checkstring(L, 3), luaL_checkstring(L, 4)) < 0)
        return raise_last(L, d->db, "link");
    lua_pushboolean(L, 1);
    return 1;
}

static int l_contains(lua_State *L) {
    LDb *d = check_db(L, 1);
    int32_t r = sekejap_contains(d->db, luaL_checkstring(L, 2));
    if (r < 0) return raise_last(L, d->db, "contains");
    lua_pushboolean(L, r != 0);
    return 1;
}

static int l_node_count(lua_State *L) {
    lua_pushinteger(L, sekejap_node_count(check_db(L, 1)->db));
    return 1;
}

static int l_edge_count(lua_State *L) {
    lua_pushinteger(L, sekejap_edge_count(check_db(L, 1)->db));
    return 1;
}

static int l_compact(lua_State *L) {
    LDb *d = check_db(L, 1);
    if (sekejap_compact(d->db) < 0) return raise_last(L, d->db, "compact");
    lua_pushboolean(L, 1);
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

static int l_stmt_gc(lua_State *L) {
    LStmt *ls = (LStmt *)luaL_checkudata(L, 1, STMT_MT);
    if (ls->stmt != NULL) {
        sekejap_stmt_free(ls->stmt);
        ls->stmt = NULL;
    }
    return 0;
}

// ── module functions ──────────────────────────────────────────────────────────

static int l_open(lua_State *L) {
    const char *path = luaL_checkstring(L, 1);
    SekejapDb *db = sekejap_open(path);
    if (db == NULL) luaL_error(L, "sekejap.open failed: %s", path);
    LDb *d = (LDb *)lua_newuserdatauv(L, sizeof(LDb), 0);
    d->db = db;
    luaL_setmetatable(L, DB_MT);
    return 1;
}

static int l_version(lua_State *L) {
    lua_pushstring(L, sekejap_version());
    return 1;
}

static const luaL_Reg db_methods[] = {
    {"execute", l_execute},
    {"execute_params", l_execute_params},
    {"query", l_query},
    {"query_params", l_query_params},
    {"prepare", l_prepare},
    {"query_prepared", l_query_prepared},
    {"put", l_put},
    {"put_many", l_put_many},
    {"get", l_get},
    {"remove", l_remove},
    {"link", l_link},
    {"contains", l_contains},
    {"node_count", l_node_count},
    {"edge_count", l_edge_count},
    {"compact", l_compact},
    {"close", l_close},
    {NULL, NULL},
};

static const luaL_Reg module_fns[] = {
    {"open", l_open},
    {"version", l_version},
    {NULL, NULL},
};

int luaopen_sekejap(lua_State *L) {
    // Database metatable: methods via __index, self-close via __gc/__close.
    luaL_newmetatable(L, DB_MT);
    lua_pushcfunction(L, l_close);
    lua_setfield(L, -2, "__gc");
    lua_pushcfunction(L, l_close);
    lua_setfield(L, -2, "__close"); // Lua 5.4 to-be-closed variables
    lua_newtable(L);
    luaL_setfuncs(L, db_methods, 0);
    lua_setfield(L, -2, "__index");
    lua_pop(L, 1);

    // Prepared-statement metatable: self-free via __gc.
    luaL_newmetatable(L, STMT_MT);
    lua_pushcfunction(L, l_stmt_gc);
    lua_setfield(L, -2, "__gc");
    lua_pop(L, 1);

    luaL_newlib(L, module_fns);
    return 1;
}
