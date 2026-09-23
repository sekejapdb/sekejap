"""README.md's Python, run exactly as it is written.

Every ```python block of the top-level README is executed in order, in one
namespace, in a fresh directory -- what a reader following the README does.
A block that raises fails this test, and the two answers the README prints
beside its code are checked against what the library returns. The SQL inside
those blocks is checked row for row, with contrasting queries, by
`dist/rust/tests/readme_correctness.rs`; this file proves the Python around it.

The pandas block is a separate program in the README (it opens `./bali`
again), so the first handle is closed before it runs, and it is skipped where
pandas cannot be imported.
"""

import gc
import os
import re

import pytest

import sekejap

README = os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", "..", "README.md")


def blocks():
    text = open(README, encoding="utf-8").read()
    return [
        (text[: m.start()].count("\n") + 2, m.group(1))
        for m in re.finditer(r"```python\n(.*?)```", text, re.S)
    ]


def test_every_python_block_runs_and_prints_what_the_readme_says(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    answers = {}
    real_query = sekejap.Db.query

    def recording(self, sql, params=None):
        rows = real_query(self, sql, params)
        answers[" ".join(sql.split())] = rows
        return rows

    monkeypatch.setattr(sekejap.Db, "query", recording)
    namespace = {}
    ran = skipped = 0
    for line, code in blocks():
        if "import pandas" in code:
            # A separate program in the README: it reopens ./bali.
            namespace.pop("db", None)
            gc.collect()
            try:
                import pandas  # noqa: F401
            except Exception:
                skipped += 1
                continue
            with open("tourists.csv", "w") as f:
                f.write("tourist_id,name,home_city\nkadek,Kadek,Denpasar\n")
        try:
            exec(compile(code, "README.md:%d" % line, "exec"), namespace)
        except Exception as error:
            pytest.fail("README.md line %d raised %s: %s" % (line, type(error).__name__, error))
        ran += 1
    assert ran + skipped == len(blocks()) and ran >= len(blocks()) - 1

    # The two answers the README prints beside its code.
    assert answers[
        "SELECT name, home_city FROM tourists WHERE home_city = 'Melbourne'"
    ] == [{"name": "Chloe", "home_city": "Melbourne"}]
    assert answers["SELECT name, arrival FROM tourists WHERE _key = 'chloe'"] == [
        {"name": "Chloe", "arrival": "2024-06-01T00:00:00Z"}
    ]
    # The one-hop pattern finds the flight the README inserted and linked.
    assert answers[
        "SELECT airline, hours FROM GRAPH_TABLE (base MATCH "
        "(t:tourists WHERE t._key = 'chloe')-[:flew_on]->(f:flights) "
        "COLUMNS (f.airline AS airline, f.duration_hours AS hours))"
    ] == [{"airline": "Qantas", "hours": 6}]
