# Python

Install from [PyPI](https://pypi.org/project/sekejap/):

```bash
pip install sekejap
```

## First query

```python
from sekejap import DB

db = DB("./mydb")            # persistent; DB() for in-memory
db.execute("CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT)")
db.execute("INSERT INTO places (_key, name) VALUES ('a', 'Uluwatu')")

for hit in db.query("SELECT * FROM places"):
    print(hit.payload)       # JSON string per row
```

## Beyond SQL

The Python API mirrors the Rust core:

```python
# Direct record + edge operations
db.put("venues/town_hall", '{"_collection":"venues","_key":"town_hall"}')
db.link("bands/the_vines", "venues/town_hall", "played_at")
db.link_meta("a", "b", "rated", '{"stars": 5}')     # edge with attributes
db.edges_from("venues/town_hall")                    # -> [EdgeHit, ...]

# Bulk loading: one disk sync for the whole batch
db.begin_bulk()
db.put_many([("t/k1", '{"_collection":"t","_key":"k1"}'), ...])
db.link_many([("t/k1", "t/k2", "likes"), ...])
db.end_bulk()

# Vectors
db.put_vector("t/k1", "emb", [0.1, 0.9, 0.0])
db.get_vector("t/k1", "emb")
db.set_hnsw_ef_search(200)      # recall/speed knob; None restores default

# Ranked text search over a bm25 index
db.bm25_search("body", "grilled healthy", 10)   # -> [(key, score), ...]

# Open modes and maintenance
DB.open_paged("./big")          # mmap topology: fast open, small memory
DB.open_read_only("./mydb")
db.memory_report()              # {structure: bytes}
db.trim_memory()
```

## pandas

The `df` accessor moves data between sekejap and DataFrames:

```python
df = db.df.query("SELECT * FROM places WHERE area = $1", ["seminyak"])
db.df.load_nodes(df, "places")                     # DataFrame → records

# Embedding columns go straight to the vector store (HNSW picks them up):
db.df.load_nodes(df, "docs", vector_cols=["embedding"])

# Edges from a DataFrame, one disk sync for the whole load:
db.df.load_edges(edges_df, source_col="s", target_col="t", edge_type="knows")
```

`create_collection` builds a typed table from a dict. Loads run inside a bulk
scope (one disk sync). pandas is only required if you use `db.df`.

A runnable five-stop tour (SQL, graph, spatial, vector, hybrid):
[`wrappers/python/examples/tour.py`](../../../wrappers/python/examples/tour.py).
