# sekejap

sekejap is a graph-first, embedded multimodel database that stores your data in several forms at once: plain records, graph relationships, geographic shapes, vectors, and full text. You can query and combine them in a single SQL statement.

It runs inside your application, like SQLite, with no separate server to install or manage. Store your database on local disk for lightweight and offline use, or use S3-compatible object storage for datasets that grow beyond a single machine.

*(“sekejap” is Indonesian for “a brief moment”, reflecting how quickly you can set it up and start working with your multimodel data.)*

It's available as a Rust/Python/Dart/Kotlin/Swift/Java/Node.js/Go library, and a command-line tool.

📖 **Documentation:** [`docs/`](docs/README.md) — a [user guide](docs/guide/README.md)
(the query language, including the [`SELECT … FROM MATCH`](docs/guide/graph-queries.md)
graph reference) and [engine internals](docs/internals/README.md).

---

## Why you might want it

Applications often need more than one kind of database at the same time:

- a **relational** store for structured records,
- a **graph** database for relationships ("who is connected to what"),
- a **spatial** index for location queries ("what's near me"),
- a **vector** store for similarity search over embeddings,
- a **full-text** search engine for matching words in text.

Running and keeping all of those in sync is a lot of moving parts. sekejap puts
them in one embedded engine behind one query language, so a single query can use
several of them together.

It's a good fit for:

- **Local and mobile apps** — runs in-process with no server and a small
  footprint, so it works offline on phones and edge devices.
- **Hybrid search and RAG** — rank results by combining vector similarity,
  geographic location, and text relevance in a single query, then follow the
  graph to pull in related records as context for a model.
- **On-device memory for AI** — an agent or robot records what it observes
  (place, time, a note, a perception vector) as it happens, and later recalls it
  by any mix of location, similarity, and relationships — a private, queryable
  memory with no network round-trip.

---

## Install

**Python** — [PyPI](https://pypi.org/project/sekejap/)

```bash
pip install sekejap                 # includes S3 support
```

**Rust** — [crates.io](https://crates.io/crates/sekejap)

```bash
cargo add sekejap                   # library
cargo add sekejap --features s3     # library, with S3 support
cargo install sekejap-cli           # command-line tool
```

**Node.js** — [npm](https://www.npmjs.com/package/sekejap)

```bash
npm install sekejap                 # prebuilt native binaries, no toolchain needed
```

**Dart / Flutter** — [pub.dev](https://pub.dev/packages/sekejap)

```bash
flutter pub add sekejap             # or: dart pub add sekejap
```

**Kotlin / Java** — [Maven Central](https://central.sonatype.com/artifact/com.zebflow/sekejap)

```kotlin
// build.gradle.kts
implementation("com.zebflow:sekejap:0.13.3")
```

---

## A first look

The examples below all use the same small dataset: some tourists, the flights
they arrived on, and places, restaurants, and dishes to visit and eat.

### 1. Create some tables

A table needs a `_key` column as its primary key. Other columns can be ordinary
types (`TEXT`, `INTEGER`, `REAL`, `TIMESTAMPTZ`) or one of the special ones:
`GEO` for geography, `VECTOR` for embeddings.

```python
from sekejap import DB

db = DB("./bali")   # a directory on disk; created if it doesn't exist

db.execute("""
    CREATE TABLE tourists (
        _key      TEXT PRIMARY KEY,
        name      TEXT,
        home_city TEXT,
        arrival   TIMESTAMPTZ,
        taste     VECTOR          -- an embedding of what this person likes
    )
""")
db.execute("CREATE TABLE flights     (_key TEXT PRIMARY KEY, airline TEXT, duration_hours INTEGER)")
db.execute("CREATE TABLE restaurants (_key TEXT PRIMARY KEY, name TEXT, area TEXT, geometry GEO)")
db.execute("""
    CREATE TABLE dishes (
        _key TEXT PRIMARY KEY, name TEXT, price INTEGER, protein_g INTEGER,
        description TEXT, geometry GEO, open_now BOOLEAN, embedding VECTOR
    )
""")
```

### 2. Add indexes for the query types you'll use

An index makes a certain kind of lookup fast. You only need the ones your
queries actually use.

```python
db.execute("CREATE INDEX ON dishes USING spatial (geometry)")     # location queries
db.execute("CREATE INDEX ON dishes USING bm25    (description)")  # text relevance
db.execute("CREATE INDEX ON tourists USING hnsw  (taste)")        # vector similarity
```

### 3. Insert data

```python
db.execute("INSERT INTO tourists (_key, name, home_city, arrival) VALUES ('chloe', 'Chloe', 'Melbourne', '2024-06-01')")
db.execute("INSERT INTO tourists (_key, name, home_city, arrival) VALUES ('aiym',  'Aiym',  'Almaty',    '2024-06-02')")

# A relationship (edge): tourist "chloe" flew on flight "qf-mel".
db.execute("INSERT ('tourists/chloe')-[:flew_on]->('flights/qf-mel')")
```

### 4. Run a query

Ordinary SQL works as you'd expect:

```python
db.query("SELECT name, home_city FROM tourists WHERE home_city = 'Melbourne'")
# → { name: "Chloe", home_city: "Melbourne" }
```

That's the whole loop: create tables, add the indexes you need, insert rows and
relationships, and query. The rest of this README shows what each data model can
do, then how to combine them.

---

## The five data models

Each section is a short, self-contained example. They build toward the last one,
where several models are used in a single query.

### Records and filters (SQL)

Standard SQL — `SELECT`, `WHERE`, `ORDER BY`, `GROUP BY`, aggregates.

```python
db.query("""
    SELECT area, COUNT(*) AS n
    FROM restaurants
    GROUP BY area
    ORDER BY n DESC
""")
```

### Relationships (graph)

A relationship between two rows is called an **edge**. You query edges with a
`MATCH` pattern inside `FROM`. Everything around the `MATCH` is ordinary SQL.

```python
# Follow one edge: which flight did Chloe arrive on?
db.query("""
    SELECT f.airline AS airline, f.duration_hours AS hours
    FROM MATCH (t:tourists)-[:flew_on]->(f:flights)
    WHERE t._key = 'chloe'
""")
```

The pattern reads left to right: start at a `tourists` row (`t`), follow a
`flew_on` edge, arrive at a `flights` row (`f`). The arrow direction matters —
`-[:e]->` follows edges forward, `<-[:e]-` follows them backward.

You can follow a chain of several hops, and `*1..3` means "between 1 and 3 hops":

```python
# Places reachable within 2 "near" hops of somewhere Chloe visited.
# DISTINCT removes duplicates when a place can be reached more than one way.
db.query("""
    SELECT DISTINCT p._key AS place
    FROM MATCH (c:tourists)-[:visited]->(m:places)-[:near*1..2]->(p:places)
    WHERE c._key = 'chloe'
""")
```

### Location (spatial)

A `GEO` column holds a shape (a point, line, or polygon). With a spatial index
you can ask distance and containment questions.

```python
# Restaurants within 5 km of a point (longitude, latitude).
db.query("""
    SELECT name FROM restaurants
    WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5.0)
""")
```

### Similarity (vector)

A `VECTOR` column holds an embedding — a list of numbers that captures the
"meaning" of something. With an HNSW index you can find the rows whose vectors
are closest to a given one.

```python
# The 5 tourists whose taste is most similar to a given taste vector.
db.query("""
    SELECT name FROM tourists
    WHERE VECTOR_NEAR(taste, [0.9, 0.1, 0.0, 0.0], 5)
""")
```

### Text (full-text)

For matching words in text, sekejap offers three tools:

- `ILIKE '%word%'` — simple substring match (fast with a `gin` index).
- `BM25(field, 'query')` — relevance scoring, like a classic search engine.
- `SEARCH('query')` — a positional search index with typo tolerance.

```python
# Dishes whose description is relevant to "grilled chicken", best first.
db.query("""
    SELECT name FROM dishes
    WHERE BM25(description, 'grilled chicken') > 0.0
    ORDER BY BM25(description, 'grilled chicken') DESC
""")
```

### Time

Timestamps are ordinary columns; a few helper functions work on them.

```python
db.query("""
    SELECT name, AGE_DAYS(arrival) AS days_here, NOW() AS current_time
    FROM tourists WHERE _key = 'chloe'
""")
# → { name: "Chloe", days_here: 5, current_time: "2024-06-06T09:00:00Z" }
```

---

## Combining models in one query

This is the point of a multi-model database: asking one question that would
otherwise need several systems.

**"What should Chloe order for delivery right now?"** — a dish that is near her,
still open, in her price range, has enough protein, matches a craving, and is
ranked by how well it fits both the words she typed and her taste.

```python
db.query("""
    SELECT r.name AS restaurant, d.name AS dish, d.price AS price
    FROM MATCH (r:restaurants)-[:serves]->(d:dishes)
    WHERE d.open_now = true
      AND d.price BETWEEN 40000 AND 90000                        -- price range (IDR)
      AND d.protein_g >= 25                                      -- enough protein
      AND ST_DWithin(d.geometry, POINT(115.168 -8.690), 5.0)     -- within 5 km
      AND BM25(d.description, 'grilled chicken healthy') > 0.0    -- matches the craving
    ORDER BY BM25(d.description, 'grilled healthy') * 0.6         -- text relevance
           + VECTOR_COSINE(d.embedding, [0.7,0.3,0.0,0.0]) * 0.4 -- taste similarity
      DESC
    LIMIT 10
""")
```

The `WHERE` clause narrows the results using the graph, spatial, scalar, and
text models. The `ORDER BY` combines a text score and a vector score into one
ranking. The whole thing is one statement.

A second example — **a personal journal** where each entry records a place, a
time, some text, and a "mood" vector. Because the entries are just rows (and can
be linked into the graph), you can search them by text, by similarity, or by
time:

```python
db.execute("""
    CREATE TABLE diary (
        _key TEXT PRIMARY KEY, author TEXT, place TEXT,
        logged_at TIMESTAMPTZ, reflection TEXT, mood VECTOR
    )
""")
db.execute("CREATE INDEX ON diary USING search (reflection)")   # search the text
db.execute("CREATE INDEX ON diary USING hnsw   (mood)")         # find similar moods

# "Where did I write about feeling small?" — text search over the entries.
db.query("""
    SELECT place, logged_at FROM diary
    WHERE author = 'chloe' AND SEARCH('small still')
    ORDER BY logged_at
""")

# "Find an earlier moment that felt like tonight." — nearest mood vector.
db.query("""
    SELECT place, reflection FROM diary
    WHERE author = 'chloe'
    ORDER BY mood <=> [0.2, 0.7, 0.1, 0.0] ASC
    LIMIT 1
""")
```

---

## Data types

| Type | SQL keyword | Stored as | Use for |
|---|---|---|---|
| Text | `TEXT` | UTF-8 string | names, categories, keys |
| Integer | `INTEGER` | 64-bit integer | prices, durations, counts |
| Float | `REAL` | 64-bit float | scores, ratings, weights |
| Boolean | `BOOLEAN` | `true` / `false` | flags, toggles (e.g. `open_now`) |
| Timestamp | `TIMESTAMPTZ` | ISO-8601 date/time | arrivals, log times |
| Geometry | `GEO` | GeoJSON shape | points, areas, routes |
| Vector | `VECTOR` | list of floats | embeddings (taste, mood, images) |
| JSON | `JSON` | arbitrary JSON | nested / unstructured data |

- **GEO** accepts any GeoJSON geometry — `Point`, `Polygon`, `LineString`, `MultiPolygon`.
- **VECTOR** is written as an array literal: `[0.12, -0.03, 0.87, ...]`.

---

## Indexes

An index speeds up one kind of query. Create only the ones you need.

| Index | `USING` keyword | Makes this fast |
|---|---|---|
| Hash | `hash` | equality: `field = 'x'`, `IN (...)` |
| B-tree | `btree` | ranges and ordering: `>`, `<`, `BETWEEN`, `ORDER BY` |
| GIN | `gin` | substring text match: `ILIKE '%pattern%'` |
| Spatial | `spatial` | location: `ST_DWithin`, `ST_Contains`, `ST_Within`, `ST_Intersects` |
| HNSW | `hnsw` | vector similarity: `VECTOR_NEAR(...)`, `<=>` ordering |
| BM25 | `bm25` | ranked text search: `BM25(field, 'query')` |
| Search | `search` | positional, typo-tolerant search: `SEARCH('query')` |

```sql
CREATE INDEX ON dishes   USING spatial (geometry)
CREATE INDEX ON dishes   USING bm25    (description)
CREATE INDEX ON diary    USING search  (reflection)
CREATE INDEX ON tourists USING hnsw    (taste)
```

All index types survive a restart. After a large bulk load, run `REINDEX` (or
`.compact` in the CLI) so later startups are fast.

---

## Interfaces

sekejap has three ways to use it. They query the same database.

### SQL

The main interface. A quick tour of what the dialect supports:

```sql
-- Schema
CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, category TEXT, geometry GEO)
ALTER TABLE places ADD COLUMN rating REAL
ALTER TABLE places RENAME COLUMN category TO kind

-- Rows
INSERT INTO places (_key, name, category) VALUES ('uluwatu', 'Uluwatu Temple', 'temple')
UPDATE places SET rating = 4.8 WHERE _key = 'uluwatu'
DELETE FROM places WHERE kind = 'closed'

-- Edges, optionally with properties
INSERT ('tourists/chloe')-[:visited {rating: 4.8, hours: 2}]->('places/uluwatu')
DELETE ('tourists/chloe')-[:visited]->('places/uluwatu')

-- Graph traversal: forward -[:e]-> and backward <-[:e]-
SELECT dest._key AS place
FROM MATCH (a:places)-[:near*1..3]->(dest:places)
WHERE a._key = 'seminyak-beach'

-- Aggregation over a pattern: COUNT / SUM / AVG / MIN / MAX, and COUNT(DISTINCT ...)
SELECT p._key AS place, COUNT(DISTINCT t.home_city) AS cities
FROM MATCH (p:places)<-[:visited]-(t:tourists)
GROUP BY p._key
ORDER BY cities DESC

-- Edge properties: a named edge (-[v:type]->) exposes its rating + metadata
SELECT t.name AS visitor, v.rating AS rating
FROM MATCH (p:places)<-[v:visited]-(t:tourists)
WHERE p._key = 'uluwatu'
ORDER BY v.rating DESC

-- Multi-stage traversal: carry a result into a follow-on MATCH with WITH
SELECT d.name AS dish, COUNT(*) AS orders
FROM MATCH (c:tourists)-[:similar_taste]->(peer:tourists)
WHERE c._key = 'chloe'
WITH peer
MATCH (peer)-[:ate]->(d:dishes)
GROUP BY d.name

-- Shortest path: 0 rows if unreachable, 1 row if a path exists
SELECT a.name AS from_n, b.name AS to_n, r.length AS hops
FROM MATCH SHORTEST (a:tourists)-[r*]->(b:dishes)
WHERE a._key = 'chloe' AND b._key = 'betutu-chicken'

-- Spatial, vector, and text
SELECT * FROM places   WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5.0)
SELECT * FROM tourists WHERE VECTOR_NEAR(taste, [0.9, 0.1, 0.0, 0.0], 5)
SELECT * FROM places   WHERE name ILIKE '%uluwatu%'

-- Transactions: all statements commit together, or none do
BEGIN
INSERT ('tourists/chloe')-[:booked]->('flights/qf-mel')
INSERT ('tourists/chloe')-[:stayed_at]->('villas/seminyak-01')
COMMIT

-- Inspect the database
SHOW TABLES
SHOW EDGES FROM tourists TO places
```

### Rust

Besides raw SQL, the Rust library has a builder API for lower-level control:

```rust
use sekejap::CoreDB;

let mut db = CoreDB::open("./bali")?;

// Restaurants within 3 km of a point (latitude, longitude, km).
let nearby = db.collection("restaurants")
    .st_dwithin(-8.690, 115.168, 3.0)
    .collect();

// Filter and sort.
let picks = db.collection("dishes")
    .where_gte("protein_g", 25.0)
    .sort("price", true)   // true = ascending
    .take(10)
    .collect();

// Add a plain edge.
db.link("tourists/chloe", "places/uluwatu", "visited");

// Add an edge with attributes (any names; primitives are stored efficiently).
db.link_meta("tourists/chloe", "places/uluwatu", "visited", r#"{"rating": 4.8, "hours": 2}"#)?;
```

### Python (with pandas)

The Python library can load from and return pandas DataFrames:

```python
import pandas as pd
from sekejap import DB

db = DB("./bali")

# Load a DataFrame as rows in a table.
df = pd.read_csv("tourists.csv")
db.df.load_nodes(df, "tourists", id_col="tourist_id",
                 mapping={"tourist_id": "_key", "full_name": "name"})

# Get query results back as a DataFrame.
result = db.df.query("SELECT * FROM dishes WHERE protein_g >= 25")
```

---

## Data larger than local disk (S3)

sekejap can keep its data on S3-compatible storage and fetch pieces on demand,
so you can query datasets bigger than the local disk. Works with AWS S3, MinIO,
Cloudflare R2, and other S3-compatible stores.

```python
from sekejap import DB

db = DB.open_s3("s3://my-bucket/bali",
                access_key_id="AKID...", secret_access_key="secret...",
                region="ap-southeast-1",
                cache_budget_bytes=256 * 1024 * 1024)   # in-memory cache size

db.query("SELECT * FROM places WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 10.0)")
```

---

## Command-line tool

```bash
sekejap                                   # in-memory session
sekejap ./bali                            # open a database on disk
sekejap ./bali "SELECT * FROM places;"    # run one statement and exit
echo "SELECT ...;" | sekejap ./bali       # pipe in a script
```

Inside the interactive session:

```
sekejap> CREATE TABLE places (_key TEXT, name TEXT, geometry GEO);
sekejap> SELECT * FROM places WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5.0);
sekejap> .tables          # list tables
sekejap> .schema places   # show a table's columns
sekejap> .compact         # compact the database after a big load
sekejap> .help
```

---

## License

MIT
