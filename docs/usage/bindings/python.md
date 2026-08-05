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

```python
# Direct record + edge operations
db.put("venues/town_hall", '{"_collection":"venues","_key":"town_hall"}')
db.link("bands/the_vines", "venues/town_hall", "played_at")

# Graph query — same SQL surface as everywhere else
db.query("SELECT v._key FROM MATCH (b:bands)-[:played_at]->(v:venues)")
```

## pandas

The `df` accessor moves data between sekejap and DataFrames:

```python
df = db.df.query("SELECT * FROM places")          # query → DataFrame
db.df.load_nodes(df, collection="places")          # DataFrame → records
```

`load_edges` and `create_collection` follow the same pattern. pandas is only
required if you use `db.df`.

A runnable five-stop tour (SQL, graph, spatial, vector, hybrid):
[`wrappers/python/examples/tour.py`](../../../wrappers/python/examples/tour.py).
