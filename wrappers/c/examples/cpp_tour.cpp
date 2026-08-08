// A tour of the C++ SDK (sekejap.hpp) — SQL, params, prepared, records, graph.
//
// Build (from the repo root, after `cargo build --release -p sekejap-capi`):
//   clang++ -std=c++17 -Iwrappers/c/include wrappers/c/examples/cpp_tour.cpp \
//     -Ltarget/release -lsekejap -Wl,-rpath,target/release -o /tmp/cpp_tour
//   /tmp/cpp_tour
#include "sekejap.hpp"

#include <cassert>
#include <cstdio>
#include <filesystem>

int main() {
    auto dir = std::filesystem::temp_directory_path() / "sekejap-cpp-tour";
    std::filesystem::remove_all(dir);

    sekejap::Db db = sekejap::Db::open(dir.string());
    std::printf("sekejap %s\n", sekejap::version().c_str());

    // Relational
    db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)");
    db.execute("INSERT INTO places (_key, name, area) VALUES ('ubud', 'Ubud', 'central')");
    db.executeParams("INSERT INTO places (_key, name, area) VALUES ($1, $2, $3)",
                     R"(["kuta","Kuta","south"])");

    std::string rows = db.query("SELECT name FROM places WHERE area = 'central'");
    std::printf("central: %s\n", rows.c_str());
    assert(rows.find("Ubud") != std::string::npos);

    // Prepared
    sekejap::Stmt byArea = db.prepare("SELECT _key FROM places WHERE area = $1");
    std::string south = db.queryPrepared(byArea, R"(["south"])");
    assert(south.find("kuta") != std::string::npos);

    // Records + graph
    db.put("places/sanur", R"({"_collection":"places","_key":"sanur","name":"Sanur","area":"south"})");
    assert(db.contains("places/sanur"));
    auto got = db.get("places/sanur");
    assert(got.has_value() && got->find("Sanur") != std::string::npos);

    auto miss = db.get("places/nowhere");
    assert(!miss.has_value());   // clean miss → nullopt, not an exception

    db.execute("CREATE TABLE tourists (_key TEXT PRIMARY KEY, name TEXT)");
    db.put("tourists/chloe", R"({"_collection":"tourists","_key":"chloe","name":"Chloe"})");
    db.link("tourists/chloe", "places/ubud", "visited");
    std::string visited = db.query(
        "SELECT p.name AS place FROM MATCH (t:tourists)-[:visited]->(p:places) WHERE t._key = 'chloe'");
    std::printf("chloe visited: %s\n", visited.c_str());
    assert(visited.find("Ubud") != std::string::npos);

    std::printf("nodes=%ld edges=%ld\n", db.nodeCount(), db.edgeCount());
    assert(db.nodeCount() >= 4 && db.edgeCount() >= 1);

    // Error path: malformed SQL throws sekejap::Error (a missing table just
    // returns an empty set, which is not an error).
    bool threw = false;
    try {
        db.query("THIS IS NOT VALID SQL");
    } catch (const sekejap::Error &e) {
        threw = true;
        std::printf("caught expected error: %s\n", e.what());
    }
    assert(threw);

    std::printf("ALL C++ SDK CHECKS PASSED\n");
    return 0;
}
