# sekejap

sekejap is a graph-first, embedded multimodel database that stores your data in several forms at once: plain records, graph relationships, geographic shapes, vectors, and full text. You can query and combine them in a single SQL statement.

It runs inside your application, like SQLite, with no separate server to install or manage. Your database is a directory on local disk.

*("sekejap" is Indonesian for "a brief moment", reflecting how quickly you can set it up and start working with your multimodel data.)*

It's available as a Rust/Python/Dart/Kotlin/Swift/Node.js/Go library, and a command-line tool.

📝 **Changelog:** [CHANGELOG.md](CHANGELOG.md) — 0.18.0 makes graph queries ISO GQL inside `GRAPH_TABLE`, writes edges in SQL through edge tables, and adds PostGIS geometry input and output and named schemas. A 0.17 database opens as it is. 0.18.1: the API and every binding list tables in named schemas and describe edge tables. 0.18.2: `LIKE`/`ILIKE` with any pattern and no setup, keys from the declared primary key, `to_tsquery` prefixes. 0.18.3: ORDER BY on any column with several keys, NULL where PostgreSQL puts it, and row comparison for "load more".

📖 **Documentation:** [`docs/lang/GQL_PROFILE.md`](docs/lang/GQL_PROFILE.md) (graph queries, with examples), [`docs/lang/QL_CONTRACT.md`](docs/lang/QL_CONTRACT.md) (the query language), [`docs/dist/RUST_API.md`](docs/dist/RUST_API.md) (the Rust surface), and [`docs/core/GRAPH_CONTRACT.md`](docs/core/GRAPH_CONTRACT.md) (edge semantics).

📊 **Benchmarks:** 50,000 rows, 44 query cases, against PostgreSQL (with PostGIS and pgvector) and SQLite (with FTS5 and R-tree). Reproduction harnesses and results live in [`bench/`](bench/).

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

## How you'll run it

One thing decides how you open it: **is the database part of your app, or is it
running as a service?**

```rust,signatures
use sekejap::Db;

// part of your app — starts and stops with it
let db = Db::open("./mydb")?;

// runs as a service — long-lived, looks after its own data
let db = Db::open_service("/var/lib/app/db")?;
```

| | what it is | example |
|---|---|---|
| **`open`** | the database starts and stops with your app | a mobile app, a game, an analysis script |
| **`open_service`** | it keeps running and looks after its data | a small server, a robot, an IoT gateway — even for one person |

`open_service` gives your app the things a database server normally does for
you: one writer, snapshot readers that never wait behind a write, and a bounded
buffer pool over long runs.

There is still **no separate server** to install, start or manage, and nothing
extra to enable at build time. These are functions your app calls, in your app's
own process — which is why it is still an embedded database.

To reach a database from another machine, use the command-line tool: `sekejap-pg`
so PostgreSQL clients can connect. See [`docs/dist/WIRE_CONTRACT.md`](docs/dist/WIRE_CONTRACT.md)
for what it answers.

---

## Install

Each binding is its own package over the same C ABI. Index: [`dist/bindings/README.md`](dist/bindings/README.md).

**Python** — [PyPI](https://pypi.org/project/sekejap/) · [source](dist/bindings/wrappers/python/)

```bash
pip install sekejap
```

**Rust** — [crates.io](https://crates.io/crates/sekejap)

```bash
cargo add sekejap
```

**Node.js / TypeScript** — [npm](https://www.npmjs.com/package/sekejap) · [source](dist/bindings/wrappers/node/)

```bash
npm install sekejap                 # prebuilt native binaries, no toolchain needed
```

**Dart / Flutter** — [pub.dev](https://pub.dev/packages/sekejap) · [source](dist/bindings/wrappers/dart/)

```bash
flutter pub add sekejap             # or: dart pub add sekejap
```

**Kotlin / Java** — [Maven Central](https://central.sonatype.com/namespace/life.sekejap) · [source](dist/bindings/wrappers/kotlin/)

```kotlin
// desktop / server JVM, Kotlin or Java, over JDK FFM downcalls
dependencies {
  implementation("life.sekejap:sekejap-ffm:0.18.3")
}
```

**Swift** — [source](dist/bindings/wrappers/swift/), a SwiftPM package over the C ABI.

**Go** — Go modules

```bash
go get github.com/sekejapdb/sekejap/dist/bindings/wrappers/go
```

**C / C++ and other native callers** — the C ABI in [`dist/ffi/`](dist/ffi/)
(builds from source; ships one header plus a static/shared library).

---

## A first look

The examples below all use the same small dataset: some tourists, the flights
they arrived on, and places, restaurants, and dishes to visit and eat.

### 1. Create some tables

A table needs a `_key` column as its primary key. Other columns can be ordinary
types (`TEXT`, `INT`, `REAL`, `TIMESTAMPTZ`) or one of the special ones:
`GEOMETRY(Point,4326)` for geography, `VECTOR(n)` for embeddings.

```python
from sekejap import DB

db = DB("./bali")   # a directory on disk; created if it doesn't exist

db.execute("""
    CREATE TABLE tourists (
        _key      TEXT PRIMARY KEY,
        name      TEXT,
        home_city TEXT,
        arrival   TIMESTAMPTZ,
        taste     VECTOR(4)        -- an embedding of what this person likes
    )
""")
db.execute("CREATE TABLE flights     (_key TEXT PRIMARY KEY, airline TEXT, duration_hours INT)")
db.execute("CREATE TABLE restaurants (_key TEXT PRIMARY KEY, name TEXT, area TEXT, geometry GEOMETRY(Point,4326))")
db.execute("""
    CREATE TABLE dishes (
        _key TEXT PRIMARY KEY, name TEXT, price INT, protein_g INT,
        description TEXT, geometry GEOMETRY(Point,4326), open_now BOOLEAN, embedding VECTOR(4)
    )
""")
```

### 2. Add the two indexes that are a choice

Ordinary columns are already indexed. `TEXT`, `INT`, `REAL`, `BOOLEAN`,
`TIMESTAMPTZ`, `DATE` and `GEOMETRY` columns get their index when the table is
created and it is kept up to date on every write, so the tables above can be
queried as they stand. There was never a decision to make about them: there is
one way to index a number, and it costs about a microsecond per row to
maintain.

Two things are a real choice, and you declare those:

```python
db.execute("CREATE INDEX ON dishes USING gin  (to_tsvector('simple', description))")  # text relevance
db.execute("CREATE INDEX ON tourists USING quantized (taste vector_cosine_ops)")      # vector similarity
db.execute("CREATE INDEX ON dishes USING exact (embedding)")                          # exact similarity
```

Full-text search brings an analyzer and an index roughly the size of the text.
A vector index is the one genuine trade in the system — an exact index answers
the true nearest set, a quantized one is an order of magnitude smaller and
approximate — and nobody can pick for you.

A `WHERE` over a column with no index is still refused rather than answered by
scanning every row -- except `LIKE` / `ILIKE`, which work on any text column
with no setup (`WHERE email ILIKE '%doe%'`) by checking each row, and say so.
`ORDER BY` works on any column, several keys at once, with or without an index:
an index lets the walk stop early, and `EXPLAIN` says which. "Load more" is
plain SQL: `WHERE (name, _key) > ($1, $2) ORDER BY name, _key LIMIT 20`. If you want a wide table to index nothing automatically,
say so once:

```python
db.execute("CREATE TABLE events (_key TEXT PRIMARY KEY, payload TEXT) WITH (index: none)")
```

The full rule, column type by column type, is in
[`docs/lang/INDEX_CONTRACT.md`](docs/lang/INDEX_CONTRACT.md).

### 3. Insert data

```python
db.execute("INSERT INTO tourists (_key, name, home_city, arrival) VALUES ('chloe', 'Chloe', 'Melbourne', '2024-06-01')")
db.execute("INSERT INTO tourists (_key, name, home_city, arrival) VALUES ('aiym',  'Aiym',  'Almaty',    '2024-06-02')")

db.execute("INSERT INTO flights (_key, airline, duration_hours) VALUES ('qf-mel', 'Qantas', 6)")

# A relationship (edge): tourist "chloe" flew on flight "qf-mel".
db.link("tourists", "chloe", "flew_on", "flights", "qf-mel")
```

As in PostgreSQL, `INSERT` of a key that already exists is refused
(`23505`); `INSERT ... ON CONFLICT (_key) DO UPDATE SET col = EXCLUDED.col`
is the upsert, and a `UNIQUE` column refuses a second equal value.

Passwords are stored with pgcrypto's functions, as in PostgreSQL:

```python
db.execute("CREATE TABLE account (_key TEXT PRIMARY KEY, email TEXT UNIQUE, password_hash TEXT)")
db.execute("INSERT INTO account (_key, email, password_hash) VALUES ($1, $2, crypt($3, gen_salt('bf')))",
           ["u1", "chloe@example.com", "correct horse"])
# At login: read the stored hash by email, then check the typed password against it.
stored_hash = db.query("SELECT password_hash FROM account WHERE email = $1", ["chloe@example.com"])[0]["password_hash"]
db.query("SELECT crypt($1, $2) = $2 AS ok", ["correct horse", stored_hash])
# → { ok: true }
```

### 4. Run a query

Ordinary SQL works as you'd expect:

```python
db.query("SELECT name, home_city FROM tourists WHERE home_city = 'Melbourne'")
# → { name: "Chloe", home_city: "Melbourne" }
```

That's the whole loop: create tables, declare the two indexes that are a
choice, insert rows and relationships, and query. The rest of this README shows what each data model can
do, then how to combine them.

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
`MATCH` pattern inside `GRAPH_TABLE`. Everything around it is ordinary SQL.

```python
# Follow one edge: which flight did Chloe arrive on?
db.query("""
    SELECT *
    FROM GRAPH_TABLE (base MATCH
        (t:tourists WHERE t._key = 'chloe')-[:flew_on]->(f:flights)
        RETURN f.airline AS airline, f.duration_hours AS hours)
""")
```

The pattern reads left to right: start at a `tourists` row (`t`), follow a
`flew_on` edge, arrive at a `flights` row (`f`). The arrow direction matters —
`-[:e]->` follows edges forward, `<-[:e]-` follows them backward.

You can follow the same kind of edge for several hops, and `{1,2}` means
"between 1 and 2 hops":

```python
# Tourists within 2 "similar_taste" hops of Chloe.
db.link("tourists", "chloe", "similar_taste", "tourists", "aiym")
db.query("""
    SELECT *
    FROM GRAPH_TABLE (base MATCH
        (c:tourists WHERE c._key = 'chloe')-[:similar_taste]->{1,2}(t:tourists)
        RETURN t.name AS tourist)
""")
```

A pattern usually starts at one row, named by its key, and walks out from
it; with no key predicate it starts by scanning the collection instead.

Edges can also be written in SQL, the way PostgreSQL, Oracle and Spanner
write a property graph's edges: an **edge table** whose `REFERENCES` columns
name the two rows an edge joins, declared in a property graph, then plain
`INSERT`, `UPDATE`, `DELETE` and `SELECT`. Its `PRIMARY KEY` decides how many
edges one pair may have, and a taken key is refused as in PostgreSQL. It is a
view over the graph's own edges, not a table of rows
([edge tables](docs/core/EDGE_TABLES.md)).

```python
# Which restaurants a tourist has visited, and what they thought of them.
db.execute("""
    CREATE TABLE visited (
        tourist TEXT REFERENCES tourists, place TEXT REFERENCES restaurants,
        rating REAL, PRIMARY KEY (tourist, place)
    )
""")
db.execute("""
    CREATE PROPERTY GRAPH bali
        VERTEX TABLES (tourists, restaurants)
        EDGE TABLES (visited SOURCE KEY (tourist) REFERENCES tourists (_key)
                             DESTINATION KEY (place) REFERENCES restaurants (_key))
""")
db.execute("INSERT INTO restaurants (_key, name, area) VALUES ('warung-sunset', 'Warung Sunset', 'Seminyak')")
db.execute("INSERT INTO visited VALUES ('chloe', 'warung-sunset', 4.5)")
db.query("SELECT place, rating FROM visited WHERE tourist = 'chloe'")
# → { place: "warung-sunset", rating: 4.5 }
```

Graph queries follow ISO GQL: each matching path is one row, so a place
reached two ways is returned twice, and `RETURN DISTINCT` returns it once.
A `WHERE` inside a node applies to that node only. The
[GQL profile](docs/lang/GQL_PROFILE.md) covers the rest: path modes,
shortest and cheapest paths, `OPTIONAL MATCH`, `EXISTS`, and text, spatial
and vector search inside a pattern.

### Location (spatial)

A `GEOMETRY` column holds a shape (a point, line, or polygon), typed with its
SRID. With a spatial index you can ask distance and containment questions.
Distances follow PostGIS: `::geography` makes them metres, so the same query
gives the same answer in both.

```python
# Restaurants within 5 km of a point (longitude, latitude).
db.query("""
    SELECT name FROM restaurants
    WHERE ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 5000.0)
""")
```

### Similarity (vector)

A `VECTOR(n)` column holds an embedding — a fixed-length list of numbers that
captures the "meaning" of something. With a vector index you can find the rows
whose vectors are closest to a given one, exactly or approximately.

Storing many vectors is mostly a question of how many dimensions you keep, not
which index you pick: see [planning vector storage](docs/core/VECTOR_CAPACITY.md).

```python
# The 5 tourists whose taste is most similar to a given taste vector.
db.query("""
    SELECT name FROM tourists
    ORDER BY taste <=> '[0.9, 0.1, 0.0, 0.0]'
    LIMIT 5
""")
```

### Text (full-text)

Full-text matching is boolean plus a ranking score, the PostgreSQL way:
`to_tsvector`/`to_tsquery` for the match, `bm25` for the ranking.

```python
# Dishes whose description matches "grilled chicken", best first.
db.query("""
    SELECT name FROM dishes
    WHERE to_tsvector('simple', description) @@ to_tsquery('simple', 'grilled & chicken')
    ORDER BY bm25(description, 'grilled chicken') DESC
""")
```

### Time

Timestamps are ordinary columns; a range or an equality on them is an ordinary
`WHERE`.

```python
db.query("""
    SELECT name, arrival
    FROM tourists WHERE _key = 'chloe'
""")
# → { name: "Chloe", arrival: "2024-06-01T00:00:00Z" }
```

## Combining models in one query

This is the point of a multi-model database: asking one question that would
otherwise need several systems.

**"What should Chloe order for delivery right now?"** — a dish that is near her,
still open, in her price range, has enough protein, matches a craving, and is
ranked by how well it fits both the words she typed and her taste.

```python
db.query("""
    SELECT name, price
    FROM dishes
    WHERE open_now = true
      AND price BETWEEN 40000 AND 90000                             -- price range (IDR)
      AND protein_g >= 25                                           -- enough protein
      AND ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 5000.0) -- within 5 km (metres)
      AND to_tsvector('simple', description) @@ to_tsquery('simple', 'grilled & healthy') -- matches the craving
    ORDER BY 0.6 * bm25(description, 'grilled healthy')             -- text relevance
           + 0.4 * (1 - (embedding <=> '[0.7,0.3,0.0,0.0]'))        -- taste similarity
      DESC
    LIMIT 10
""")
```

The `WHERE` clause narrows the results using the spatial, scalar, and text
models. The `ORDER BY` combines a text score and a vector score into one
ranking. The whole thing is one statement.

A second example — **a personal journal** where each entry records a place, a
time, some text, and a "mood" vector. Because the entries are just rows (and can
be linked into the graph), you can search them by text, by similarity, or by
time:

```python
db.execute("""
    CREATE TABLE diary (
        _key TEXT PRIMARY KEY, author TEXT, place TEXT,
        logged_at TIMESTAMPTZ, reflection TEXT, mood VECTOR(4)
    )
""")
db.execute("CREATE INDEX ON diary USING gin       (to_tsvector('simple', reflection))")  # search the text
db.execute("CREATE INDEX ON diary USING quantized (mood vector_cosine_ops)")             # find similar moods
# `author`, `place` and `logged_at` were indexed by the CREATE TABLE above.

# "Where did I write about feeling small?" — text search over the entries.
db.query("""
    SELECT place, logged_at FROM diary
    WHERE author = 'chloe'
      AND to_tsvector('simple', reflection) @@ to_tsquery('simple', 'small & still')
    ORDER BY logged_at
""")

# "Find an earlier moment that felt like tonight." — nearest mood vector.
db.query("""
    SELECT place, reflection FROM diary
    WHERE author = 'chloe'
    ORDER BY mood <=> '[0.2, 0.7, 0.1, 0.0]' ASC
    LIMIT 1
""")
```

## Data types

| Type | SQL keyword | Stored as | Use for |
|---|---|---|---|
| Text | `TEXT` | UTF-8 string | names, categories, keys |
| Integer | `INT` | 64-bit integer | prices, durations, counts |
| Float | `REAL` / `DOUBLE PRECISION` | 64-bit float | scores, ratings, weights |
| Boolean | `BOOLEAN` | `true` / `false` | flags, toggles (e.g. `open_now`) |
| Timestamp | `TIMESTAMPTZ` | ISO-8601 date/time | arrivals, log times |
| Geometry | `GEOMETRY(kind,SRID)` | typed shape | points, areas, routes |
| Vector | `VECTOR(n)` | fixed-length list of floats | embeddings (taste, mood, images) |
| JSON | `JSONB` | arbitrary JSON | nested / unstructured data |

- **`GEOMETRY`** is typed with its kind and SRID, e.g. `GEOMETRY(Point,4326)`, `GEOMETRY(Polygon,4326)`.
- **`VECTOR(n)`** is written as an array literal of exactly `n` numbers: `'[0.12, -0.03, 0.87, ...]'`.

## Indexes

An index speeds up one kind of query. Most of them are already there.

| Index | `USING` keyword | Makes this fast | |
|---|---|---|---|
| Hash | `hash` | equality: `field = 'x'`, `IN (...)` | automatic |
| B-tree | `btree` | ranges and ordering: `>`, `<`, `BETWEEN`, `ORDER BY` | automatic |
| GiST | `gist` | location: `ST_DWithin`, `ST_Contains`, `ST_Within`, `ST_Intersects` | automatic |
| GIN | `gin` over `to_tsvector(...)` | boolean text match and BM25 ranking | declared |
| Exact vector | `exact` | exact nearest neighbour: `<=>`, `<->`, `<#>` ordering | declared |
| Quantized vector | `quantized` (aliases: `hnsw`, `ivfflat`) | approximate nearest neighbour over compact codes | declared |
| Vamana graph | `vamana` (alias: `diskann`) | approximate nearest neighbour by a graph walk that skips most rows | declared |

```sql,tour
CREATE INDEX ON dishes   USING gin        (to_tsvector('simple', description))
CREATE INDEX ON tourists USING quantized  (taste vector_cosine_ops)
```

An AUTOMATIC index is created with the table: an ordinary column has one
implementation and one obvious answer, so you are not asked. A DECLARED one is
a trade you have to make — an analyzer and a large index for full text, size
against exactness for vectors. `WITH (index: none)` on the `CREATE TABLE` turns
the automatic ones off, and `WITH (index: [a, b])` keeps only the columns you
name; `docs/lang/INDEX_CONTRACT.md` is where the line is drawn and why.

All index types survive a restart and update as you write, so a build only
happens once — at `CREATE TABLE` for an automatic one, at `CREATE INDEX` for a
declared one.

## Interfaces

sekejap has three ways to use it. They query the same database.

### SQL

The main interface. A quick tour of what the dialect supports:

```sql,tour
-- Schema
CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, category TEXT, geometry GEOMETRY(Point,4326))
ALTER TABLE places ADD COLUMN rating REAL

-- Rows
INSERT INTO places (_key, name, category) VALUES ('uluwatu', 'Uluwatu Temple', 'temple')
UPDATE places SET rating = 4.8 WHERE _key = 'uluwatu'
DELETE FROM places WHERE category = 'closed'

-- Graph traversal: forward -[:e]-> and backward <-[:e]-
SELECT *
FROM GRAPH_TABLE (base MATCH
    (a:places WHERE a._key = 'seminyak-beach')-[:near]->{1,3}(dest:places)
    RETURN dest._key AS place)

-- Aggregation: COUNT / SUM / AVG / MIN / MAX, grouped
SELECT category, COUNT(*) AS n, AVG(rating) AS avg_rating
FROM places
GROUP BY category
ORDER BY n DESC

-- Edge properties: an inline WHERE on the edge, and its fields in RETURN
SELECT *
FROM GRAPH_TABLE (base MATCH
    (p:places WHERE p._key = 'uluwatu')<-[v:visited]-(t:tourists)
    RETURN t.name AS visitor, v.rating AS rating)

-- Spatial, vector, and text
SELECT * FROM places   WHERE ST_DWithin(geometry, ST_MakePoint(115.168, -8.690)::geography, 5000.0)
SELECT * FROM tourists ORDER BY taste <=> '[0.9, 0.1, 0.0, 0.0]' LIMIT 5

-- Inspect the database
SHOW TABLES
SHOW EDGES
```

`JOIN` has no atomic behind it and is refused by name rather than emulated — a
graph pattern is how sekejap joins.

### Rust

One handle, one error, SQL and a typed row API:

```rust,signatures
use sekejap::{Db, Direction};
use serde_json::json;

let db = Db::open("./bali")?;

// Restaurants within 3 km of a point (longitude, latitude, metres).
let nearby = db.query(
    "SELECT name FROM restaurants WHERE ST_DWithin(geometry, ST_MakePoint($1, $2)::geography, $3)",
    &[json!(115.168), json!(-8.690), json!(3000.0)],
)?;

// Add a plain edge.
db.link(("tourists", "chloe"), "visited", ("places", "uluwatu"))?;

// Add an edge with properties.
db.link_with(
    ("tourists", "chloe"), "visited", ("places", "uluwatu"),
    &json!({"rating": 4.8, "hours": 2}),
)?;

// Walk it back out.
let visited = db.neighbours(("tourists", "chloe"), Some("visited"), Direction::Outgoing, 16)?;
```

### Python (with pandas)

The Python library can load from and return pandas DataFrames:

```python
import pandas as pd
from sekejap import DB

db = DB("./bali")

# Load a DataFrame as rows in a table.
df = pd.read_csv("tourists.csv")
db.df.put(df, "tourists", key_column="tourist_id")

# Get query results back as a DataFrame.
result = db.df.query("SELECT * FROM dishes WHERE protein_g >= 25")
```

## Command-line tool

```bash
sekejap ./bali                            # open a database on disk
sekejap ./bali "SELECT * FROM places;"    # run one statement and exit
```

## License

MIT OR Apache-2.0
