# Cross-wrapper micro-benchmark, Python (PyO3). See examples/bench_native.rs.
#
# Note: PyO3's query() returns native Python objects (a list of Hits), so — unlike
# the C-ABI bindings that receive a JSON string — Python *materializes* the result,
# closer to native Rust's collect(). Its overhead is PyO3 object marshalling.
import os
import tempfile
import time

from sekejap import DB

d = tempfile.mkdtemp(prefix="skbench-py-")
db = DB(d)
db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)")
for i in range(1000):
    db.execute(f"INSERT INTO t (_key, v) VALUES ('k{i}', {i})")

n = int(os.environ.get("N", "50000"))
sql = "SELECT v FROM t WHERE _key = 'k500'"
db.query(sql)  # warm

t = time.perf_counter()
for _ in range(n):
    db.query(sql)
el = time.perf_counter() - t
print(f"python {n / el:.0f} {el * 1e6 / n:.3f}")
