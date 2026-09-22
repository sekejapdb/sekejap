"""sekejap behind a FastAPI service -- the database runs in the web process.

sekejap is embedded: there is no database server to deploy beside this one.
The handle is opened in SERVICE mode, which is what gives parallel readers a
published snapshot, a statement timeout and the commit-time change feed
(`docs/dist/OPS_CONTRACT.md` §1-§5); ``sekejap::Db`` is ``Send + Sync``, so
one handle serves every request.

Run::

    pip install sekejap fastapi uvicorn
    SEKEJAP_DIRECTORY=./data uvicorn fastapi_app:app --port 8000

Try::

    curl 'localhost:8000/venues'
    curl 'localhost:8000/venues/the_tote'
    curl 'localhost:8000/venues?suburb=Fitzroy'
    curl 'localhost:8000/bands/the_vines/played_at'
"""

import os
from contextlib import asynccontextmanager

from fastapi import FastAPI, HTTPException

from sekejap import Db, Direction, SekejapError

db = None

VENUES = [
    {"name": "name", "kind": "text"},
    {"name": "suburb", "kind": "text"},
    {"name": "capacity", "kind": "int"},
]
BANDS = [{"name": "name", "kind": "text"}]


@asynccontextmanager
async def lifespan(app: FastAPI):
    global db
    directory = os.environ.get("SEKEJAP_DIRECTORY", "./data")
    db = Db.open_service(directory)
    # A statement that runs longer than a second is refused rather than
    # holding a request open: service mode is what makes that available.
    db.statement_timeout_ms(1_000)
    if not db.collections():
        db.create_collection("venues", VENUES)
        db.create_collection("bands", BANDS)
        db.put_many(
            "venues",
            {
                "the_tote": {"name": "The Tote", "suburb": "Collingwood", "capacity": 300},
                "old_bar": {"name": "The Old Bar", "suburb": "Fitzroy", "capacity": 120},
            },
        )
        db.put_many("bands", {"the_vines": {"name": "The Vines"}})
        db.link("bands", "the_vines", "played_at", "venues", "the_tote")
    try:
        yield
    finally:
        db.close()


app = FastAPI(lifespan=lifespan)


@app.get("/venues")
def venues(suburb: str | None = None):
    """Every venue, or the ones in one suburb. The filter is a $1 parameter."""
    if suburb is None:
        return db.query("SELECT _key, name, capacity FROM venues")
    return db.query(
        "SELECT _key, name, capacity FROM venues WHERE suburb = $1", [suburb]
    )


@app.get("/venues/{key}")
def venue(key: str):
    """One document by collection and key. A MISS is a 404, not an error."""
    document = db.get("venues", key)
    if document is None:
        raise HTTPException(status_code=404, detail="no venue %r" % key)
    return document


@app.post("/venues/{key}")
def put_venue(key: str, document: dict):
    """One write, committed before the response is built."""
    try:
        db.put("venues", key, document)
    except SekejapError as failure:
        raise HTTPException(status_code=400, detail=failure.message) from failure
    return {"written": key}


@app.get("/bands/{key}/played_at")
def played_at(key: str):
    """The venues one hop away, along the `played_at` edge."""
    return db.neighbours("bands", key, "played_at", Direction.OUTGOING)


@app.get("/storage")
def storage():
    """The bytes on disk, straight from the store."""
    return db.storage()
