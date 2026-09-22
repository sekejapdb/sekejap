# Cross-wrapper micro-benchmark, Python (ctypes over the C ABI).
#
# What it measures is the wrapper's round trip: one `sekejap_query` across the
# boundary, one JSON string back, one `json.loads` in Python. Unlike the 0.16
# PyO3 extension it replaces, nothing here materialises rows as native objects
# inside the library -- the answer is the JSON the C ABI hands every wrapper,
# and the parse is Python's own.
#
# Run:  PYTHONPATH=wrappers/python/python N=50000 python3 wrappers/python/bench.py
import os
import tempfile
import time

from sekejap import Db

directory = tempfile.mkdtemp(prefix="skbench-py-")
db = Db(directory)
db.create_collection("t", [{"name": "v", "kind": "int"}])
db.put_many("t", {"k%d" % i: {"v": i} for i in range(1000)})

rounds = int(os.environ.get("N", "50000"))
sql = "SELECT v FROM t WHERE _key = $1"
parameters = ["k500"]
db.query(sql, parameters)  # warm: parse, compile, cache the plan

started = time.perf_counter()
for _ in range(rounds):
    db.query(sql, parameters)
elapsed = time.perf_counter() - started
db.close()

print("python %.0f %.3f" % (rounds / elapsed, elapsed * 1e6 / rounds))
