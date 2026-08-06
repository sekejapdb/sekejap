"""sekejap + FastAPI — an embedded multi-model database behind a web API.

The engine runs in-process with the server: no database service to deploy.

Run:
    pip install sekejap fastapi uvicorn
    uvicorn fastapi_app:app --port 8000

Try:
    curl 'localhost:8000/near?m=20000'
    curl 'localhost:8000/similar'
    curl 'localhost:8000/search?q=grilled+healthy'
"""
import json
from contextlib import asynccontextmanager

from fastapi import FastAPI
from sekejap import DB

db: DB | None = None


@asynccontextmanager
async def lifespan(app: FastAPI):
    global db
    db = DB()  # in-memory demo; DB("./data") for a persistent directory
    db.execute(
        "CREATE TABLE places (_key TEXT PRIMARY KEY, name TEXT, "
        "description TEXT, geometry GEO, emb VECTOR)"
    )
    db.execute("CREATE INDEX ON places USING spatial (geometry)")
    db.execute("CREATE INDEX ON places USING hnsw (emb)")
    db.execute("CREATE INDEX ON places USING bm25 (description)")
    rows = [
        ("uluwatu", "Uluwatu Temple", "clifftop sea temple, sunset kecak dance",
         115.087, -8.829, [1.0, 0.0]),
        ("kuta", "Kuta Beach", "long sandy beach, surf schools, sunsets",
         115.168, -8.720, [0.0, 1.0]),
        ("ubud", "Ubud Center", "rice terraces, healthy cafes, yoga",
         115.263, -8.507, [0.9, 0.1]),
    ]
    db.begin_bulk()
    for key, name, desc, lon, lat, emb in rows:
        db.put(f"places/{key}", json.dumps({
            "_collection": "places", "_key": key, "name": name,
            "description": desc,
            "geometry": {"type": "Point", "coordinates": [lon, lat]},
        }))
        db.put_vector(f"places/{key}", "emb", emb)
    db.end_bulk()
    yield
    db.close()


app = FastAPI(lifespan=lifespan)


@app.get("/near")
def near(m: float = 5000.0):
    """Places within m metres of Uluwatu."""
    hits = db.query(
        "SELECT name FROM places WHERE ST_DWithin(geometry, POINT(115.087 -8.829), $1)",
        [m],
    )
    return [json.loads(h.payload) for h in hits]


@app.get("/similar")
def similar():
    """The 2 places most similar to the [1, 0] embedding."""
    hits = db.query("SELECT name FROM places WHERE VECTOR_NEAR(emb, [1.0, 0.0], 2)")
    return [json.loads(h.payload) for h in hits]


@app.get("/search")
def search(q: str):
    """Ranked text search over descriptions."""
    return [{"key": k, "score": s} for k, s in db.bm25_search("description", q, 10)]
