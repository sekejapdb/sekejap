/* A five-stop tour of sekejap from C: SQL, graph, spatial, vector, hybrid.
 *
 *   cargo build --release -p sekejap-capi     (once, from the repo root)
 *   cd wrappers/c/examples && make tour && ./tour
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include "sekejap.h"

static void show(SekejapDb *db, const char *label, const char *sql) {
    char *rows = sekejap_query(db, sql);
    printf("%s\n  %s\n", label, rows ? rows : "(error)");
    sekejap_string_free(rows);
}

int main(void) {
    char dir[] = "/tmp/sekejap-tour-XXXXXX";
    if (!mkdtemp(dir)) return 1;
    SekejapDb *db = sekejap_open(dir);
    if (!db) return 1;

    /* 1. Core SQL */
    sekejap_execute(db, "CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)");
    sekejap_execute(db, "INSERT INTO places (_key, name, area) VALUES ('uluwatu', 'Uluwatu Temple', 'south')");
    sekejap_execute(db, "INSERT INTO places (_key, name, area) VALUES ('kuta', 'Kuta Beach', 'south')");
    sekejap_execute(db, "INSERT INTO places (_key, name, area) VALUES ('ubud', 'Ubud Center', 'central')");
    show(db, "places per area:",
         "SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC");

    /* 2. Graph */
    sekejap_execute(db, "CREATE TABLE tourists (_key TEXT PRIMARY KEY, name TEXT)");
    sekejap_execute(db, "CREATE TABLE flights (_key TEXT PRIMARY KEY, airline TEXT)");
    sekejap_execute(db, "INSERT INTO tourists (_key, name) VALUES ('chloe', 'Chloe')");
    sekejap_execute(db, "INSERT INTO flights (_key, airline) VALUES ('qf-mel', 'Qantas')");
    sekejap_execute(db, "INSERT ('tourists/chloe')-[:flew_on]->('flights/qf-mel')");
    show(db, "Chloe's flight:",
         "SELECT f.airline AS airline FROM MATCH (t:tourists)-[:flew_on]->(f:flights) WHERE t._key = 'chloe'");

    /* 3. Spatial (radius in metres) */
    sekejap_execute(db, "CREATE TABLE spots (_key TEXT PRIMARY KEY, name TEXT, geometry GEO)");
    sekejap_execute(db, "CREATE INDEX ON spots USING spatial (geometry)");
    sekejap_put(db, "spots/uluwatu",
        "{\"_collection\":\"spots\",\"_key\":\"uluwatu\",\"name\":\"Uluwatu Temple\","
        "\"geometry\":{\"type\":\"Point\",\"coordinates\":[115.087,-8.829]}}");
    sekejap_put(db, "spots/kuta",
        "{\"_collection\":\"spots\",\"_key\":\"kuta\",\"name\":\"Kuta Beach\","
        "\"geometry\":{\"type\":\"Point\",\"coordinates\":[115.168,-8.720]}}");
    sekejap_put(db, "spots/ubud",
        "{\"_collection\":\"spots\",\"_key\":\"ubud\",\"name\":\"Ubud Center\","
        "\"geometry\":{\"type\":\"Point\",\"coordinates\":[115.263,-8.507]}}");
    show(db, "within 20 km of Uluwatu:",
         "SELECT name FROM spots WHERE ST_DWithin(geometry, POINT(115.087 -8.829), 20000.0)");

    /* 4. Vector */
    sekejap_execute(db, "CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT, emb VECTOR)");
    sekejap_execute(db, "CREATE INDEX ON items USING hnsw (emb)");
    sekejap_execute(db, "INSERT INTO items (_key, name, emb) VALUES ('a', 'apple', [1.0, 0.0, 0.0])");
    sekejap_execute(db, "INSERT INTO items (_key, name, emb) VALUES ('b', 'banana', [0.0, 1.0, 0.0])");
    sekejap_execute(db, "INSERT INTO items (_key, name, emb) VALUES ('c', 'cherry', [0.9, 0.1, 0.0])");
    show(db, "2 nearest to [1,0,0]:",
         "SELECT name FROM items WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 2)");

    /* 5. Hybrid: text + vector in one ORDER BY */
    sekejap_execute(db, "CREATE TABLE dishes (_key TEXT PRIMARY KEY, name TEXT, description TEXT, embedding VECTOR)");
    sekejap_execute(db, "CREATE INDEX ON dishes USING bm25 (description)");
    sekejap_execute(db, "CREATE INDEX ON dishes USING hnsw (embedding)");
    sekejap_execute(db, "INSERT INTO dishes (_key, name, description, embedding) VALUES ('a', 'Grilled Chicken', 'healthy grilled chicken with herbs', [1.0, 0.0, 0.0])");
    sekejap_execute(db, "INSERT INTO dishes (_key, name, description, embedding) VALUES ('b', 'Fried Rice', 'classic fried rice street food', [0.0, 1.0, 0.0])");
    sekejap_execute(db, "INSERT INTO dishes (_key, name, description, embedding) VALUES ('c', 'Grilled Fish', 'grilled fish, light and healthy', [0.8, 0.2, 0.0])");
    show(db, "ranked dishes:",
         "SELECT name FROM dishes WHERE BM25(description, 'grilled healthy') > 0.0 "
         "ORDER BY BM25_NORM(description, 'grilled healthy') * 0.6 "
         "+ VECTOR_COSINE(embedding, [1.0, 0.0, 0.0]) * 0.4 DESC");

    sekejap_close(db);
    return 0;
}
