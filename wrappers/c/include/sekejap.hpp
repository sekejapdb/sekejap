// sekejap.hpp — header-only, idiomatic C++ wrapper over the sekejap C ABI.
//
// RAII handles (no manual close/free), std::string in/out, std::optional for a
// missing node, and exceptions carrying sekejap_last_error() on failure. Include
// this instead of sekejap.h to get a C++-native surface; it links the same
// libsekejap.
//
//   #include "sekejap.hpp"
//   sekejap::Db db = sekejap::Db::open("./data");
//   db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, n INTEGER)");
//   std::string rows = db.query("SELECT * FROM t WHERE n >= 1");
//
// Requires C++17. Link with -lsekejap (see wrappers/c: `make install`, or
// pkg-config sekejap).
#pragma once

#include "sekejap.h"

#include <optional>
#include <stdexcept>
#include <string>
#include <utility>

namespace sekejap {

// Thrown on any C ABI failure; carries sekejap_last_error() when available.
class Error : public std::runtime_error {
public:
    explicit Error(const std::string &msg) : std::runtime_error(msg) {}
};

namespace detail {
// Copy an owned C string into std::string and free it (per the C ABI contract).
inline std::string take(char *c) {
    if (c == nullptr) return {};
    std::string s(c);
    sekejap_string_free(c);
    return s;
}
} // namespace detail

// A prepared (compiled) query. Move-only; frees itself.
class Stmt {
    friend class Db;
    SekejapStmt *s_ = nullptr;
    explicit Stmt(SekejapStmt *s) : s_(s) {}

public:
    Stmt(const Stmt &) = delete;
    Stmt &operator=(const Stmt &) = delete;
    Stmt(Stmt &&o) noexcept : s_(std::exchange(o.s_, nullptr)) {}
    Stmt &operator=(Stmt &&o) noexcept {
        if (this != &o) {
            if (s_) sekejap_stmt_free(s_);
            s_ = std::exchange(o.s_, nullptr);
        }
        return *this;
    }
    ~Stmt() {
        if (s_) sekejap_stmt_free(s_);
    }
};

// An open sekejap database. Move-only; closes itself.
class Db {
    SekejapDb *db_ = nullptr;

    explicit Db(SekejapDb *db) : db_(db) {}

    std::string lastError() const { return detail::take(sekejap_last_error(db_)); }

    [[noreturn]] void fail(const char *what) const {
        std::string e = lastError();
        throw Error(e.empty() ? std::string(what) : e);
    }

    static Db opened(SekejapDb *db, const char *what) {
        if (db == nullptr) throw Error(std::string(what) + " failed");
        return Db(db);
    }

public:
    // ── open / lifecycle ────────────────────────────────────────────────────
    static Db open(const std::string &path) {
        return opened(sekejap_open(path.c_str()), "sekejap_open");
    }
    static Db openPaged(const std::string &path) {
        return opened(sekejap_open_paged(path.c_str()), "sekejap_open_paged");
    }
    static Db openReadOnly(const std::string &path) {
        return opened(sekejap_open_read_only(path.c_str()), "sekejap_open_read_only");
    }

    Db(const Db &) = delete;
    Db &operator=(const Db &) = delete;
    Db(Db &&o) noexcept : db_(std::exchange(o.db_, nullptr)) {}
    Db &operator=(Db &&o) noexcept {
        if (this != &o) {
            if (db_) sekejap_close(db_);
            db_ = std::exchange(o.db_, nullptr);
        }
        return *this;
    }
    ~Db() {
        if (db_) sekejap_close(db_);
    }

    // ── statements & queries ────────────────────────────────────────────────
    long execute(const std::string &sql) {
        long n = sekejap_execute(db_, sql.c_str());
        if (n < 0) fail("execute");
        return n;
    }
    long executeParams(const std::string &sql, const std::string &paramsJson) {
        long n = sekejap_execute_params(db_, sql.c_str(), paramsJson.c_str());
        if (n < 0) fail("executeParams");
        return n;
    }
    std::string query(const std::string &sql) {
        char *r = sekejap_query(db_, sql.c_str());
        if (r == nullptr) fail("query");
        return detail::take(r);
    }
    std::string queryParams(const std::string &sql, const std::string &paramsJson) {
        char *r = sekejap_query_params(db_, sql.c_str(), paramsJson.c_str());
        if (r == nullptr) fail("queryParams");
        return detail::take(r);
    }

    Stmt prepare(const std::string &sql) {
        SekejapStmt *s = sekejap_prepare(db_, sql.c_str());
        if (s == nullptr) fail("prepare");
        return Stmt(s);
    }
    std::string queryPrepared(const Stmt &stmt, const std::string &paramsJson) {
        char *r = sekejap_query_prepared(db_, stmt.s_, paramsJson.c_str());
        if (r == nullptr) fail("queryPrepared");
        return detail::take(r);
    }

    // ── records & graph ─────────────────────────────────────────────────────
    void put(const std::string &slug, const std::string &payloadJson) {
        if (sekejap_put(db_, slug.c_str(), payloadJson.c_str()) < 0) fail("put");
    }
    long putMany(const std::string &rowsJson) {
        long n = sekejap_put_many(db_, rowsJson.c_str());
        if (n < 0) fail("putMany");
        return n;
    }
    // Returns the node's JSON payload, or std::nullopt if it does not exist.
    std::optional<std::string> get(const std::string &slug) {
        char *r = sekejap_get(db_, slug.c_str());
        if (r != nullptr) return detail::take(r);
        std::string e = lastError();          // null: clean miss clears last_error
        if (!e.empty()) throw Error(e);
        return std::nullopt;
    }
    void remove(const std::string &slug) {
        if (sekejap_remove(db_, slug.c_str()) < 0) fail("remove");
    }
    void link(const std::string &from, const std::string &to, const std::string &edgeType) {
        if (sekejap_link(db_, from.c_str(), to.c_str(), edgeType.c_str()) < 0) fail("link");
    }
    void unlink(const std::string &from, const std::string &to, const std::string &edgeType) {
        if (sekejap_unlink(db_, from.c_str(), to.c_str(), edgeType.c_str()) < 0) fail("unlink");
    }
    bool contains(const std::string &slug) {
        int32_t r = sekejap_contains(db_, slug.c_str());
        if (r < 0) fail("contains");
        return r != 0;
    }

    // ── introspection & maintenance ─────────────────────────────────────────
    long nodeCount() { return sekejap_node_count(db_); }
    long edgeCount() { return sekejap_edge_count(db_); }
    std::string collectionNames() { return detail::take(sekejap_collection_names(db_)); }
    std::string schemaDdl(const std::string &collection) {
        return detail::take(sekejap_schema_ddl(db_, collection.c_str()));
    }
    void compact() {
        if (sekejap_compact(db_) < 0) fail("compact");
    }
    void sync() {
        if (sekejap_sync(db_) < 0) fail("sync");
    }
    void trimMemory() { sekejap_trim_memory(db_); }
};

// Library version, e.g. "0.16.2". (Static string — not owned.)
inline std::string version() { return std::string(sekejap_version()); }

} // namespace sekejap
