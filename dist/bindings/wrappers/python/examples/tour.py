"""A five-stop tour of sekejap from Python: SQL, graph, spatial, vector, hybrid.

Run:  PYTHONPATH=wrappers/python/python python3 wrappers/python/examples/tour.py
(or just `python3 tour.py` once sekejap is pip-installed)
"""
import json
from sekejap import DB

db = DB()  # in-memory; DB("./mydb") for a persistent directory

# ── 1. Core SQL: CREATE / INSERT / GROUP BY ──────────────────────────────────
db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, area TEXT)")
for key, name, area in [
    ("uluwatu", "Uluwatu Temple", "south"),
    ("kuta", "Kuta Beach", "south"),
    ("ubud", "Ubud Center", "central"),
]:
    db.execute(f"INSERT INTO places (_key, name, area) VALUES ('{key}', '{name}', '{area}')")

print("places per area:")
for hit in db.query("SELECT area, COUNT(*) AS n FROM places GROUP BY area ORDER BY n DESC"):
    print("  ", hit.payload)

# ── 2. Graph: an edge + MATCH traversal ──────────────────────────────────────
db.execute("CREATE TABLE tourists (_key TEXT PRIMARY KEY, name TEXT)")
db.execute("CREATE TABLE flights (_key TEXT PRIMARY KEY, airline TEXT)")
db.execute("INSERT INTO tourists (_key, name) VALUES ('chloe', 'Chloe')")
db.execute("INSERT INTO flights (_key, airline) VALUES ('qf-mel', 'Qantas')")
db.execute("INSERT ('tourists/chloe')-[:flew_on]->('flights/qf-mel')")

print("Chloe's flight:")
for hit in db.query(
    "SELECT f.airline AS airline FROM MATCH (t:tourists)-[:flew_on]->(f:flights) "
    "WHERE t._key = 'chloe'"
):
    print("  ", hit.payload)

# ── 3. Spatial: GEO column + radius search (metres) ──────────────────────────
db.execute("CREATE TABLE spots (_key TEXT PRIMARY KEY, name TEXT, geometry GEO)")
db.execute("CREATE INDEX ON spots USING spatial (geometry)")
for key, name, lon, lat in [
    ("uluwatu", "Uluwatu Temple", 115.087, -8.829),
    ("kuta", "Kuta Beach", 115.168, -8.720),
    ("ubud", "Ubud Center", 115.263, -8.507),
]:
    db.put(f"spots/{key}", json.dumps({
        "_collection": "spots", "_key": key, "name": name,
        "geometry": {"type": "Point", "coordinates": [lon, lat]},
    }))

print("within 20 km of Uluwatu:")
for hit in db.query(
    "SELECT name FROM spots WHERE ST_DWithin(geometry, POINT(115.087 -8.829), 20000.0)"
):
    print("  ", hit.payload)

# ── 4. Vector: HNSW similarity search ────────────────────────────────────────
db.execute("CREATE TABLE items (_key TEXT PRIMARY KEY, name TEXT, emb VECTOR)")
db.execute("CREATE INDEX ON items USING hnsw (emb)")
for key, name, emb in [
    ("a", "apple", "[1.0, 0.0, 0.0]"),
    ("b", "banana", "[0.0, 1.0, 0.0]"),
    ("c", "cherry", "[0.9, 0.1, 0.0]"),
]:
    db.execute(f"INSERT INTO items (_key, name, emb) VALUES ('{key}', '{name}', {emb})")

print("2 nearest to [1,0,0]:")
for hit in db.query("SELECT name FROM items WHERE VECTOR_NEAR(emb, [1.0, 0.0, 0.0], 2)"):
    print("  ", hit.payload)

# ── 5. Hybrid: text relevance + vector similarity in one ORDER BY ────────────
db.execute(
    "CREATE TABLE dishes (_key TEXT PRIMARY KEY, name TEXT, description TEXT, embedding VECTOR)"
)
db.execute("CREATE INDEX ON dishes USING bm25 (description)")
db.execute("CREATE INDEX ON dishes USING hnsw (embedding)")
for key, name, desc, emb in [
    ("a", "Grilled Chicken", "healthy grilled chicken with herbs", "[1.0, 0.0, 0.0]"),
    ("b", "Fried Rice", "classic fried rice street food", "[0.0, 1.0, 0.0]"),
    ("c", "Grilled Fish", "grilled fish, light and healthy", "[0.8, 0.2, 0.0]"),
]:
    db.execute(
        f"INSERT INTO dishes (_key, name, description, embedding) "
        f"VALUES ('{key}', '{name}', '{desc}', {emb})"
    )

print("ranked dishes:")
for hit in db.query(
    "SELECT name FROM dishes WHERE BM25(description, 'grilled healthy') > 0.0 "
    "ORDER BY BM25_NORM(description, 'grilled healthy') * 0.6 "
    "+ VECTOR_COSINE(embedding, [1.0, 0.0, 0.0]) * 0.4 DESC"
):
    print("  ", hit.payload)
