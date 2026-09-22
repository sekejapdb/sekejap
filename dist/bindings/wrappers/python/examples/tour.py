"""A tour of sekejap 0.17 from Python: documents, SQL, edges, a walk, a batch.

Every stop is a call on the C ABI through the ctypes wrapper. Nothing here is
in memory: sekejap is disk-first, so the tour opens a directory and says where
it is.

Run::

    SEKEJAP_LIBRARY=/path/to/libsekejap.dylib \\
    PYTHONPATH=dist/bindings/wrappers/python/python \\
    python3 dist/bindings/wrappers/python/examples/tour.py [directory]

(or just ``python3 tour.py`` once ``pip install sekejap`` has put the library
inside the package).
"""

import sys
import tempfile

import sekejap
from sekejap import Db, Direction, Refused


def heading(text):
    print("\n%s\n%s" % (text, "-" * len(text)))


def main(argv):
    directory = argv[1] if len(argv) > 1 else tempfile.mkdtemp(prefix="sekejap-tour-")
    print(
        "sekejap %s, disk format %d, in %s"
        % (sekejap.version(), sekejap.format_version(), directory)
    )
    print("library: %s" % sekejap.library_path())

    with Db(directory) as db:
        # 1. The catalog: a collection is DECLARED before a document lands in
        #    it, because one document implies no column kinds for the next.
        heading("1. declare two collections")
        db.create_collection(
            "venues",
            [
                {"name": "name", "kind": "text"},
                {"name": "suburb", "kind": "text"},
                {"name": "capacity", "kind": "int"},
            ],
        )
        db.create_collection(
            "bands",
            [
                {"name": "name", "kind": "text"},
                {"name": "sound", "kind": "vector", "dimension": 3},
            ],
        )
        print("collections:", db.collections())

        # 2. Documents, addressed by collection AND key. A put commits before
        #    it returns; put_many puts a whole batch under one commit.
        heading("2. write documents")
        db.put(
            "venues",
            "fitzroy_town_hall",
            {"name": "Fitzroy Town Hall", "suburb": "Fitzroy", "capacity": 900},
        )
        written = db.put_many(
            "venues",
            {
                "the_tote": {"name": "The Tote", "suburb": "Collingwood", "capacity": 300},
                "corner_hotel": {
                    "name": "Corner Hotel",
                    "suburb": "Richmond",
                    "capacity": 800,
                },
            },
        )
        print("put_many wrote %d rows under one commit" % written)
        print("get:", db.get("venues", "the_tote"))
        print("a miss is None, not an error:", db.get("venues", "nowhere"))

        # 3. SQL over the same rows, with $n parameters.
        heading("3. query with a parameter")
        for row in db.query(
            "SELECT _key, name, capacity FROM venues WHERE _key = $1",
            ["corner_hotel"],
        ):
            print(row)
        moved = db.execute(
            "INSERT INTO bands (_key, name, sound) VALUES ($1, $2, $3)",
            ["the_vines", "The Vines", [1.0, 0.0, 0.0]],
        )
        print("insert moved %d row" % moved)

        # 4. A prepared statement: parsed once, compiled at its first bind,
        #    rebound for every set of parameters after that.
        heading("4. prepare once, rebind many")
        with db.prepare("SELECT name FROM venues WHERE _key = $1") as statement:
            print("rebindable before the first bind:", statement.rebindable)
            for key in ("fitzroy_town_hall", "the_tote", "corner_hotel"):
                print(" ", key, "->", statement.query([key])[0]["name"])
            print("rebindable after it:", statement.rebindable)

        # 5. A walk in stable id order, one bounded page at a time.
        heading("5. walk the collection a page at a time")
        with db.scan("venues", page_rows=2) as walk:
            for number, page in enumerate(walk.pages(), start=1):
                print("page %d: %s" % (number, [row["_key"] for row in page]))

        # 6. Typed edges between rows, and the rows one hop away. Both
        #    endpoints must exist: a missing one is an error, never a
        #    dangling identity.
        heading("6. link two rows and read the neighbours")
        db.put_many("bands", {"rvg": {"name": "RVG", "sound": [0.0, 1.0, 0.0]}})
        db.link("bands", "the_vines", "played_at", "venues", "the_tote")
        db.link("bands", "rvg", "played_at", "venues", "the_tote")
        for neighbour in db.neighbours("bands", "the_vines", "played_at", Direction.OUTGOING):
            print("the_vines ->", neighbour["collection"] + "/" + neighbour["key"])
        played = db.neighbours("venues", "the_tote", "played_at", Direction.INCOMING)
        print("the_tote <-", [row["key"] for row in played])

        # 7. Many writes, one barrier. A clean exit commits; an exception
        #    rolls the whole batch back.
        heading("7. a transaction commits or rolls back as one")
        with db.transaction() as tx:
            tx.put("venues", "gasometer", {"name": "The Gasometer", "suburb": "Collingwood"})
            tx.put("venues", "old_bar", {"name": "The Old Bar", "suburb": "Fitzroy"})
        print("after the commit:", db.count_rows("venues"), "rows")

        try:
            with db.transaction() as tx:
                tx.put("venues", "never_built", {"name": "Never Built"})
                raise RuntimeError("the block did not finish")
        except RuntimeError as stopped:
            print("rolled back because:", stopped)
        print("after the rollback:", db.count_rows("venues"), "rows")

        # 8. A vector order, answered by the exact-vector index family.
        heading("8. k nearest by vector")
        db.execute("CREATE INDEX bands_sound ON bands USING exact (sound)")
        nearest = db.query(
            "SELECT _key, name FROM bands ORDER BY sound <-> $1 LIMIT 1", [[1.0, 0.0, 0.0]]
        )
        print("nearest to [1, 0, 0]:", nearest)

        # 9. What is there, and what it costs on disk.
        heading("9. the catalog and the bytes")
        shape = db.describe("venues")
        print(
            "venues: %d rows, fields %s"
            % (shape["rows"], [field["name"] for field in shape["fields"]])
        )
        print("edges (a walk, and named as one):", db.scan_count_edges())
        storage = db.storage()
        print(
            "storage: %d bytes of data, %d bytes of log"
            % (storage["data_bytes"], storage["wal_bytes"])
        )

        # 10. A construct with no atomic is refused by NAME, with a reason.
        heading("10. a refusal arrives with a reason")
        try:
            db.compact()
        except Refused as refusal:
            print("compact ->", refusal.message)
        try:
            sekejap.open_memory()
        except Refused as refusal:
            print("open_memory ->", refusal.message)

    print("\nclosed. The database is still in %s" % directory)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
