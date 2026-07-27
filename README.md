# sekejap

Embedded, graph-first multi-model database. Graph traversal, spatial search, vector similarity, and full-text search — composable in a single query, zero external services, runs in-process or against S3.

*`sekejap` means **"a brief moment"** in Indonesian.* The world flies into one island, you explore it across every dimension, and the days run out fast. This README is a Bali holiday — Chloe's, mostly.

**Built for workloads that need more than one data model at a time:**

- travel & discovery — "near me, loved by people like me, described as *quiet sunset*, still open"
- hybrid RAG — find semantically similar records then walk their graph context
- local AI memory — a companion or robot that records where it went, when, and how it felt
- spatiotemporal intelligence — who was where, connected to what, when

Available as a Rust library, Rust CLI, and Python library.

📖 **Documentation:** [`docs/`](docs/README.md) — a [user guide](docs/guide/README.md) (query language, including the [`SELECT … FROM MATCH`](docs/guide/graph-queries.md) reference) and [engine internals](docs/internals/README.md).

### Who it's for

| You are a… | You'll care about |
|---|---|
| **Data scientist** | pandas ↔ DataFrame, embeddings, similarity, aggregation over graphs |
| **Full-stack developer** | one SQL surface for CRUD, search, transactions, hybrid ranking |
| **Mobile developer** | embedded & offline, "near me" spatial, tiny footprint, no server |
| **Embodied-AI developer** | a local memory graph — observations that fuse place, time, text, vector |

---

## Getting started — the world lands in Bali

Six travellers, five continents, one island: Giulia (Milan), Ethan (Toronto), Yasmine (Casablanca), Lucas (São Paulo), Aiym (Almaty), and **Chloe (Melbourne)**.

```python
from sekejap import DB

db = DB("./bali")

db.execute("""
    CREATE TABLE tourists (
        _key      TEXT PRIMARY KEY,
        name      TEXT,
        home_city TEXT,
        arrival   TIMESTAMPTZ,
        departure TIMESTAMPTZ,
        taste     VECTOR
    )
""")
db.execute("CREATE TABLE flights (_key TEXT PRIMARY KEY, airline TEXT, origin_city TEXT, duration_hours INTEGER)")
db.execute("CREATE TABLE places  (_key TEXT PRIMARY KEY, name TEXT, category TEXT, area TEXT, geometry GEO, description TEXT, embedding VECTOR)")
db.execute("CREATE TABLE restaurants (_key TEXT PRIMARY KEY, name TEXT, area TEXT, geometry GEO, open_now BOOLEAN)")
db.execute("CREATE TABLE dishes  (_key TEXT PRIMARY KEY, name TEXT, price INTEGER, protein_g INTEGER, description TEXT, geometry GEO, open_now BOOLEAN, embedding VECTOR)")

db.execute("CREATE INDEX ON places      USING spatial (geometry)")
db.execute("CREATE INDEX ON dishes      USING spatial (geometry)")
db.execute("CREATE INDEX ON dishes      USING bm25    (description)")
db.execute("CREATE INDEX ON tourists    USING hnsw    (taste)")

db.execute("INSERT INTO tourists (_key, name, home_city, arrival, departure) VALUES ('aiym',  'Aiym',  'Almaty',    '2024-06-02', '2024-06-10')")
db.execute("INSERT INTO tourists (_key, name, home_city, arrival, departure) VALUES ('chloe', 'Chloe', 'Melbourne', '2024-06-01', '2024-06-08')")

# tourist -[:flew_on]-> flight
db.execute("INSERT ('tourists/aiym')-[:flew_on]->('flights/ky-alm')")
db.execute("INSERT ('tourists/chloe')-[:flew_on]->('flights/qf-mel')")
```

### 1 — The basics: Aiym flies in from Almaty

Follow one edge. `MATCH` names the graph pattern; everything around it is ordinary SQL.

```python
db.query("""
    SELECT f.airline AS airline, f.duration_hours AS hours
    FROM MATCH (t:tourists)-[:flew_on]->(f:flights)
    WHERE t._key = 'aiym'
""")
# → { airline: "Air Astana", hours: 11 }
```

### 2 — One query, every model: what should Chloe order right now?

Food for delivery: **near** her villa, **still open**, in a **price range**, with enough **protein**, matching a **craving** — ranked by text relevance *and* taste. Graph + spatial + text + scalar filters + hybrid score, in a single statement.

```python
db.query("""
    SELECT r.name AS restaurant, d.name AS dish, d.price AS price, d.protein_g AS protein
    FROM MATCH (r:restaurants)-[:serves]->(d:dishes)
    WHERE d.open_now = true
      AND d.price >= 40000 AND d.price <= 90000                    -- IDR range
      AND d.protein_g >= 25                                        -- macro goal
      AND ST_DWithin(d.geometry, POINT(115.168 -8.690), 5.0)       -- within 5 km of her villa
      AND BM25(d.description, 'grilled chicken healthy') > 0.0      -- the craving
    ORDER BY BM25_NORM(d.description, 'grilled healthy protein') * 0.6
           + VECTOR_COSINE(d.embedding, chloe_taste)          * 0.4 DESC
    LIMIT 10
""")
# → Ayam Bakar (La Favela, 65k, 38g) ranked above Tuna Poke Bowl; the far Ubud
#   dish and the low-protein snack are filtered out.
```

### 3 — Multi-hop analysis: what did Chloe's fellow travellers fall for?

Walk **backward** across the shared inbound flight to everyone who took it, then out to the dishes they loved — and count the distinct fans.

```python
db.query("""
    SELECT d.name AS dish, COUNT(DISTINCT peer._key) AS fans
    FROM MATCH (chloe:tourists)-[:flew_on]->(f:flights)<-[:flew_on]-(peer:tourists)-[:ate]->(d:dishes)
    WHERE chloe._key = 'chloe'
    GROUP BY d.name
    ORDER BY fans DESC
""")
# → Ayam Bakar (2), Babi Guling (1)
```

That's the shape of everything below: **selection** (graph + filters) narrows the world, **ranking** scores what's left.

---

## Exploring Bali in five dimensions

### Map — spatial

```python
# Temples & beaches within 5 km of Uluwatu
db.query("SELECT * FROM places WHERE ST_DWithin(geometry, POINT(115.087 -8.829), 5.0)")
```

### Connections — graph (forward, backward, DISTINCT)

```python
# Forward: places Chloe reached within 2 hops of her itinerary
db.query("""
    SELECT DISTINCT p._key AS place
    FROM MATCH (c:tourists)-[:visited]->(m:places)-[:near*1..2]->(p:places)
    WHERE c._key = 'chloe'
""")

# Backward `<-`: who visited Uluwatu? (walk against the arrow)
db.query("""
    SELECT DISTINCT t.name AS visitor
    FROM MATCH (p:places)<-[:visited]-(t:tourists)
    WHERE p._key = 'uluwatu'
""")

# Edge properties: a bound edge exposes its strength + metadata (fixed single hops)
db.query("""
    SELECT t.name AS visitor, v.strength AS rating
    FROM MATCH (p:places)<-[v:visited]-(t:tourists)
    WHERE p._key = 'uluwatu'
    ORDER BY v.strength DESC
""")
```

> Multi-hop returns **one row per path** by default (a place reached two ways appears twice). Add `DISTINCT` for unique nodes, or `COUNT(DISTINCT field)` to count them.

### Taste — vector

```python
# Tourists whose taste is closest to Chloe's
db.query("""
    SELECT * FROM tourists
    WHERE VECTOR_NEAR(taste, chloe_taste, 5)
""")
```

### Words — full-text (BM25 relevance, or positional SEARCH)

```python
db.query("""
    SELECT * FROM places
    WHERE BM25(description, 'clifftop sunset temple') > 0.2
    ORDER BY BM25(description, 'clifftop sunset temple') DESC
""")
```

### Time — the sekejap

```python
# Day-of-trip. A seven-day holiday is a brief moment.
db.query("""
    SELECT t.name AS name, AGE_DAYS(t.arrival) AS days_here, NOW() AS this_moment
    FROM MATCH (t:tourists) WHERE t._key = 'chloe'
""")
# → { name: "Chloe", days_here: 5, this_moment: 1717... }   "the last light"
```

---

## The Spatiotemporal Diary — Chloe

Chloe keeps a diary. Each entry is a moment: **where** (a place, or a restaurant), **when** (`logged_at`), **what she wrote** (`reflection`), and **how it felt** (`mood` vector). The entries are part of the graph — `chloe -[:wrote]-> entry -[:at]-> place`, and a place may be a `restaurant -[:serves]-> dish` — so the diary can answer questions about the island it touched.

```python
db.execute("""
    CREATE TABLE diary (
        _key       TEXT PRIMARY KEY,
        author     TEXT,
        place      TEXT,
        logged_at  TIMESTAMPTZ,
        reflection TEXT,
        mood       VECTOR
    )
""")
db.execute("CREATE INDEX ON diary USING search (reflection)")   # search her own words
db.execute("CREATE INDEX ON diary USING hnsw   (mood)")         # moments that feel alike
```

```python
# Her whole week, retraced through space and time
db.query("""
    SELECT e.place AS place, e.logged_at AS moment, e.reflection AS words
    FROM MATCH (o:tourists)-[:wrote]->(e:diary)
    WHERE o._key = 'chloe'
    ORDER BY e.logged_at ASC
""")

# The moment she ate near a temple — traced through the graph (diary → warung → dish)
db.query("""
    SELECT e.logged_at AS moment, w.name AS warung, d.name AS dish
    FROM MATCH (e:diary)-[:at]->(w:restaurants)-[:serves]->(d:dishes)
    WHERE e.author = 'chloe'
    ORDER BY e.logged_at
""")

# A moment tonight that rhymes with an earlier one (nearest mood)
db.query(f"""
    SELECT place, reflection FROM diary
    WHERE author = 'chloe'
    ORDER BY mood <=> {tonight} ASC
    LIMIT 1
""")
# → this last Uluwatu sunset rhymes with the first quiet morning in Ubud.

# "Where did I write about feeling small?" — search her reflections
db.query("""
    SELECT place, logged_at FROM diary
    WHERE author = 'chloe' AND SEARCH('small still')
    ORDER BY logged_at
""")
```

The island held still while the week ran out.

---

## Data Types

| Type | SQL keyword | Stored as | Use for |
|---|---|---|---|
| Text | `TEXT` | UTF-8 string | names, categories, keys |
| Integer | `INTEGER` | i64 | prices (IDR), durations, counts |
| Float | `REAL` | f64 | scores, ratings, weights |
| Timestamp | `TIMESTAMPTZ` | ISO-8601 | arrival, departure, `logged_at` |
| Geometry | `GEO` | GeoJSON object | temple points, area polygons |
| Vector | `VECTOR` | `[f32, ...]` array | taste, mood, review embeddings |
| JSON | `JSON` | arbitrary JSON | nested / unstructured |

**GEO** accepts any GeoJSON geometry — `Point`, `Polygon`, `LineString`, `MultiPolygon`.
**VECTOR** is inserted as a SQL array literal: `[0.12, -0.03, 0.87, ...]`.

---

## Indexes

| Index | `USING` keyword | Enables |
|---|---|---|
| Hash | `hash` | `field = 'val'`, `IN (...)`, equality lookups |
| B-tree | `btree` | `>`, `<`, `BETWEEN`, `ORDER BY field` |
| GIN | `gin` | `ILIKE '%pattern%'` (exact trigram postings) |
| Spatial | `spatial` | `ST_DWithin`, `ST_Contains`, `ST_Within`, `ST_Intersects` |
| HNSW | `hnsw` | `VECTOR_NEAR(field, [...], k)`, `<=>` ordering, `VECTOR_COSINE(...)` in scores |
| BM25 | `bm25` | `BM25(field, 'query') > score`, `ORDER BY BM25(...)`, `BM25_NORM(...)` scores |
| Search | `search` | `SEARCH('query')` filter, `SEARCH_SCORE('query')` ranking (positional inverted index) |

```sql
CREATE INDEX ON places  USING spatial (geometry)
CREATE INDEX ON dishes  USING bm25    (description)
CREATE INDEX ON diary   USING search  (reflection)
CREATE INDEX ON tourists USING hnsw   (taste)
```

Or inline in `CREATE TABLE ... WITH (...)`:

```sql
CREATE TABLE dishes (
    _key TEXT PRIMARY KEY, name TEXT, price INTEGER, protein_g INTEGER,
    description TEXT, geometry GEO, embedding VECTOR
) WITH (range: ['price'], spatial: ['geometry'], bm25: ['description'], vector: ['embedding'])
```

All index types survive a cold restart. Hash, B-tree, GIN, and BM25 rebuild from persisted schema hints on open; HNSW and Spatial are stored in the snapshot. Run `REINDEX` after large bulk loads.

---

## Interfaces

sekejap has three interfaces. Use whichever fits the context.

### SQL

```sql
-- Schema
CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, category TEXT, geometry GEO)
ALTER TABLE places ADD COLUMN rating REAL
ALTER TABLE places RENAME COLUMN category TO kind

-- Mutations
INSERT INTO places (_key, name, category) VALUES ('uluwatu', 'Uluwatu Temple', 'temple')
UPDATE places SET rating = 4.8 WHERE _key = 'uluwatu'
DELETE FROM places WHERE kind = 'closed'

-- Edges (with metadata)
INSERT ('tourists/chloe')-[:visited {rating: 4.8, hours: 2}]->('places/uluwatu')
DELETE ('tourists/chloe')-[:visited]->('places/uluwatu')

-- Graph traversal — forward `-[:e]->` and backward `<-[:e]-`
SELECT dest._key AS place
FROM MATCH (a:places)-[:near*1..3]->(dest:places)
WHERE a._key = 'seminyak-beach'

-- Backward: every place that routes into Ubud
SELECT src._key AS place
FROM MATCH (dest:places)<-[:near*1..3]-(src:places)
WHERE dest._key = 'ubud'

-- DISTINCT — multi-hop returns one row per PATH; DISTINCT = unique nodes
SELECT DISTINCT dest._key AS place
FROM MATCH (a:places)-[:near*1..2]->(dest:places)
WHERE a._key = 'seminyak-beach'

-- Aggregation: COUNT / SUM / AVG / MIN / MAX / COUNT(DISTINCT field).
-- Without GROUP BY an aggregate returns exactly one row (even if nothing matches).
SELECT p._key AS place,
       COUNT(*)                        AS visits,
       COUNT(DISTINCT t.home_city)     AS cities,
       AVG(v.strength)                 AS avg_rating
FROM MATCH (p:places)<-[v:visited]-(t:tourists)
GROUP BY p._key
ORDER BY visits DESC
LIMIT 10

-- Edge properties — a bound edge (`-[v:type]->`) exposes `strength` + JSON
-- metadata. Available on FIXED single hops (not variable-length `*a..b`).
SELECT t.name AS visitor, v.strength AS rating, v.hours AS stayed
FROM MATCH (p:places)<-[v:visited]-(t:tourists)
WHERE p._key = 'uluwatu'
ORDER BY v.strength DESC

-- Multi-hop: dishes eaten by travellers whose taste matches Chloe's
SELECT d.name AS dish, COUNT(*) AS orders
FROM MATCH (c:tourists)-[:similar_taste]->(peer:tourists)-[:ate]->(d:dishes)
WHERE c._key = 'chloe'
GROUP BY d.name
ORDER BY orders DESC

-- Multi-stage with WITH — carry a binding into a follow-on MATCH
SELECT d.name AS dish, COUNT(*) AS orders
FROM MATCH (c:tourists)-[:similar_taste]->(peer:tourists)
WHERE c._key = 'chloe'
WITH peer
MATCH (peer)-[:ate]->(d:dishes)
GROUP BY d.name
ORDER BY orders DESC

-- Shortest path — 0 rows = unreachable, 1 row = found (path fields via r.*)
SELECT a.name AS from_n, b.name AS to_n, r.length AS hops, r._path_keys AS trail
FROM MATCH SHORTEST (a)-[r*]->(b)
WHERE a._key = 'tourists/chloe' AND b._key = 'dishes/babi-guling'
-- Path predicates (ANY / ALL / NONE / SINGLE) filter intermediate nodes:
AND ALL(n IN nodes(r) WHERE n.open_now = true)

-- CASE WHEN — conditional expression on a field
SELECT d.name AS dish,
       CASE WHEN d.protein_g >= 30 THEN 'high protein' ELSE 'light' END AS tier
FROM MATCH (r:restaurants)-[:serves]->(d:dishes)
WHERE r._key = 'la-favela'

-- Time: NOW(), AGE_DAYS(var.field), AGE_HOURS(var.field)  (in SELECT FROM MATCH)
SELECT t.name AS name, AGE_DAYS(t.arrival) AS days_here, NOW() AS this_moment
FROM MATCH (t:tourists) WHERE t._key = 'chloe'

-- Spatial
SELECT * FROM places WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5.0)
SELECT * FROM zones  WHERE ST_Contains(geometry, POINT(115.087 -8.829))

-- Vector
SELECT * FROM tourists WHERE VECTOR_NEAR(taste, [0.9, 0.1, 0.0, 0.0], 5)

-- Full-text: GIN (fast exact ILIKE) · BM25 (ranked) · SEARCH (positional, typo-tolerant)
SELECT * FROM places WHERE name ILIKE '%uluwatu%'
SELECT * FROM places WHERE BM25(description, 'sunset temple') > 0.3
    ORDER BY BM25(description, 'sunset temple') DESC
SELECT *, SEARCH_SCORE('quiet still') AS relevance FROM diary
    WHERE SEARCH('quiet still') ORDER BY SEARCH_SCORE('quiet still') DESC

-- Hybrid ranking — combine any signals with +, -, *, /, ()
ORDER BY BM25_NORM(description, 'quiet sunset') * 0.5
       + VECTOR_COSINE(embedding, [0.7,0.3,0.0,0.0]) * 0.5 DESC
ORDER BY -ST_DISTANCE_KM(geometry, POINT(115.168 -8.690)) DESC   -- nearest first
-- vector distance operators: <=> cosine, <-> L2, <#> dot, <+> L1

-- Transactions — book a whole trip atomically
BEGIN
INSERT ('tourists/chloe')-[:booked]->('flights/qf-mel')
INSERT ('tourists/chloe')-[:stayed_at]->('villas/seminyak-01')
INSERT ('tourists/chloe')-[:joined]->('activities/uluwatu-kecak')
COMMIT   -- all three, or none

-- Introspection
SHOW TABLES
SHOW EDGES
SHOW EDGES FROM tourists TO places
SHOW places
```

### Atomic (Rust fluent builder)

For lower-level control, offline/mobile inner loops, or pre-resolved hashes.

```rust
use sekejap::CoreDB;

let mut db = CoreDB::open("./bali")?;

// "Near me" — spatial radius, embedded and offline
let nearby = db.collection("restaurants")
    .st_dwithin(-8.690, 115.168, 3.0)   // lat, lon, km
    .collect();

// Fluent scan with filters
let picks = db.collection("dishes")
    .where_gte("protein_g", 25)
    .order_by("price", false)   // false = ascending
    .limit(10)
    .collect();

// Edges
db.link("tourists/chloe", "places/uluwatu", "visited", 4.8);
db.link_meta("tourists/chloe", "places/uluwatu", "visited", 4.8, r#"{"hours":2}"#)?;
```

### Python DataFrame (`db.df`)

For data-science workflows — load from CSV/parquet, get results back as DataFrames.

```python
import pandas as pd
from sekejap import DB

db = DB("./bali")

# Load tourists + review embeddings
df = pd.read_csv("tourists.csv")
db.df.load_nodes(df, "tourists", id_col="tourist_id",
                 mapping={"tourist_id": "_key", "full_name": "name"})

db.df.load_edges(pd.read_csv("visits.csv"),
                 source_col="tourist_id", target_col="place_id",
                 edge_type="visited",
                 source_collection="tourists", target_collection="places",
                 weight_col="rating")

# Query → DataFrame
df = db.df.query("SELECT * FROM dishes WHERE protein_g >= 25 AND price <= 90000")
```

---

## 📡 IoT — the island, sensed

sekejap is embedded and continuously-writable, so an edge device can log sensor streams and query them locally — no server round-trip. Sensors monitor places; readings are just nodes and edges.

```python
db.execute("CREATE TABLE sensors (_key TEXT PRIMARY KEY, kind TEXT, geometry GEO, reading REAL, updated_at TIMESTAMPTZ)")
# sensor -[:monitors]-> place
db.execute("INSERT ('sensors/crowd-kuta')-[:monitors]->('places/kuta-beach')")

# Quietest beaches near Seminyak right now (low crowd sensors, close by)
db.query("""
    SELECT b._key AS beach, s.reading AS crowd
    FROM MATCH (s:sensors)-[:monitors]->(b:places)
    WHERE s.kind = 'crowd'
      AND s.reading < 0.4
      AND ST_DWithin(b.geometry, POINT(115.168 -8.690), 8.0)
    ORDER BY s.reading ASC
""")
```

---

## 🤖 Embodied AI — a humanoid travel assistant

A companion robot walks Bali with Chloe and keeps a **local memory**: each observation fuses **place, time, a note, and a perception embedding**. Because sekejap holds graph + vector + spatial + text in one embedded engine, "recall" is a single query — the kind of local memory an on-device assistant needs.

```python
db.execute("""
    CREATE TABLE observations (
        _key       TEXT PRIMARY KEY,
        seen_at    TIMESTAMPTZ,
        geometry   GEO,          -- where the assistant was
        note       TEXT,         -- what it noticed
        embedding  VECTOR        -- what it perceived (image/scene vector)
    )
""")
db.execute("CREATE INDEX ON observations USING spatial (geometry)")
db.execute("CREATE INDEX ON observations USING hnsw    (embedding)")
db.execute("CREATE INDEX ON observations USING search  (note)")

# "What did we see near Ubud yesterday?"  (spatial + temporal)
db.query("""
    SELECT note, seen_at FROM observations
    WHERE ST_DWithin(geometry, POINT(115.263 -8.507), 3.0)
    ORDER BY seen_at DESC
""")

# "Find a past sunset that looked like this one"  (vector recall)
db.query(f"""
    SELECT note, seen_at FROM observations
    ORDER BY embedding <=> {current_scene} ASC
    LIMIT 3
""")

# "What did we say about temples?"  (text recall)
db.query("SELECT note, seen_at FROM observations WHERE SEARCH('temple offering incense')")
```

Continuous ingest + local hybrid recall, in-process, on a small device — the same engine, no companion services.

---

## S3 Remote Storage

Query datasets larger than local disk. Payloads stay on S3, fetched on demand via block-level caching.

```python
from sekejap import DB

db = DB.open_s3("s3://my-bucket/bali",
                access_key_id="AKID...", secret_access_key="secret...",
                region="ap-southeast-1",
                cache_budget_bytes=256 * 1024 * 1024,   # RAM cache
                cache_dir="/tmp/sekejap-cache")          # optional disk cache

hits = db.query("SELECT * FROM places WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 10.0)")
```

Works with AWS S3, MinIO, Cloudflare R2, and any S3-compatible store (`endpoint` / `allow_http` for custom endpoints).

---

## Installation

```bash
cargo add sekejap                 # Rust library
cargo add sekejap --features s3   # with S3 support
cargo install sekejap-cli         # Rust CLI
pip install sekejap               # Python (includes S3)
```

## CLI

```bash
sekejap                              # in-memory REPL
sekejap ./bali                       # persistent REPL
sekejap ./bali "SELECT * FROM places;"   # one-shot
echo "SELECT ...;" | sekejap ./bali  # pipe a script

sekejap> CREATE TABLE places (_key TEXT, name TEXT, geometry GEO);
sekejap> SELECT * FROM places WHERE ST_DWithin(geometry, POINT(115.168 -8.690), 5.0);
sekejap> .tables        # introspection dot-commands
sekejap> .edges tourists
sekejap> .schema places
sekejap> .help
```

## License

MIT
