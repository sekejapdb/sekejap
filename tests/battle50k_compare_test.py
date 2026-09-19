#!/usr/bin/env python3
"""Tests for tools/battle50k_compare.py.

NOT RUN as part of this change -- written as source only, per the worker
brief's HARD RULES (no test/benchmark execution). stdlib only (unittest +
subprocess), so it can be run later with:

    python3 tests/battle50k_compare_test.py
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
COMPARE_SCRIPT = REPO_ROOT / "tools" / "battle50k_compare.py"


def make_report(arm, rows, commit, stages, cases, deviations=None):
    return {
        "arm": arm,
        "commit": commit,
        "rows": rows,
        "data": "queries.jsonl",
        "queries": "queries.json",
        "stages": stages,
        "cases": cases,
        "deviations": deviations or [],
    }


AGREEING_E4 = make_report(
    "e4",
    50000,
    "abc1234",
    stages=[
        {"name": "load", "ms": 1000.0},
        {"name": "index:place_loc_gist", "ms": 50.0},
        {"name": "disk_bytes", "bytes": 10 * 1024 * 1024},
    ],
    cases=[
        {
            "name": "kind_eq",
            "kind": "filter",
            "queries": 50,
            "median_us": 100.0,
            "p90_us": 150.0,
            "total_rows": 4200,
            "first_keys": ["k1", "k2"],
            "k": None,
            "recall_at_k": None,
            "note": "",
        },
        {
            "name": "knn_10",
            "kind": "ranked",
            "queries": 50,
            "median_us": 200.0,
            "p90_us": 250.0,
            "total_rows": 500,
            "first_keys": ["a", "b", "c"],
            "k": 10,
            "recall_at_k": None,
            "note": "",
        },
        {
            "name": "vec_ann_10",
            "kind": "approx",
            "queries": 50,
            "median_us": 300.0,
            "p90_us": 350.0,
            "total_rows": 500,
            "first_keys": ["a"],
            "k": 10,
            "recall_at_k": 0.95,
            "note": "",
        },
        {
            "name": "hybrid_blend_10",
            "kind": "ranked",
            "queries": 50,
            "median_us": None,
            "p90_us": None,
            "total_rows": 0,
            "first_keys": [],
            "k": 10,
            "recall_at_k": None,
            "note": "deviation: E4 has no score expression",
        },
    ],
)

AGREEING_PG = make_report(
    "postgres",
    50000,
    "abc1234",
    stages=[
        {"name": "load", "ms": 900.0},
        {"name": "index:place_loc_gist", "ms": 40.0},
        {"name": "disk_bytes", "bytes": 12 * 1024 * 1024},
    ],
    cases=[
        {
            "name": "kind_eq",
            "kind": "filter",
            "queries": 50,
            "median_us": 80.0,
            "p90_us": 120.0,
            "total_rows": 4200,
            "first_keys": ["k1", "k2"],
            "k": None,
            "recall_at_k": None,
            "note": "",
        },
        {
            "name": "knn_10",
            "kind": "ranked",
            "queries": 50,
            "median_us": 220.0,
            "p90_us": 260.0,
            "total_rows": 500,
            "first_keys": ["a", "b", "x"],
            "k": 10,
            "recall_at_k": None,
            "note": "",
        },
        {
            "name": "vec_ann_10",
            "kind": "approx",
            "queries": 50,
            "median_us": 90.0,
            "p90_us": 120.0,
            "total_rows": 500,
            "first_keys": ["a"],
            "k": 10,
            "recall_at_k": 0.91,
            "note": "",
        },
        {
            "name": "hybrid_blend_10",
            "kind": "ranked",
            "queries": 50,
            "median_us": 400.0,
            "p90_us": 450.0,
            "total_rows": 500,
            "first_keys": ["a", "b"],
            "k": 10,
            "recall_at_k": None,
            "note": "",
        },
    ],
)

# Same as AGREEING_PG but with a differing total_rows on the filter case,
# to exercise the DISAGREE / exit-1 path.
DISAGREEING_PG = json.loads(json.dumps(AGREEING_PG))
DISAGREEING_PG["cases"][0]["total_rows"] = 4199


class Battle50kCompareTest(unittest.TestCase):
    def _run(self, e4_report, pg_report, extra_args=None):
        with tempfile.TemporaryDirectory() as tmp:
            e4_path = Path(tmp) / "e4.json"
            pg_path = Path(tmp) / "postgres.json"
            e4_path.write_text(json.dumps(e4_report))
            pg_path.write_text(json.dumps(pg_report))
            args = [sys.executable, str(COMPARE_SCRIPT), str(e4_path), str(pg_path)]
            args.extend(extra_args or [])
            return subprocess.run(args, capture_output=True, text=True)

    def test_agreeing_reports_exit_zero_and_verdict_line(self):
        result = self._run(AGREEING_E4, AGREEING_PG)
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        verdict_lines = [
            line for line in result.stdout.splitlines() if line.startswith("VERDICT:")
        ]
        self.assertEqual(len(verdict_lines), 1)
        # 1 filter case (kind_eq), agreeing.
        self.assertIn("filter cases agreeing 1/1", verdict_lines[0])
        # 3 cases with both medians present (kind_eq, knn_10, vec_ann_10);
        # hybrid_blend_10 has E4 median_us null and is excluded.
        self.assertIn("cases with both medians present", verdict_lines[0])

    def test_disagreeing_filter_case_exits_one(self):
        result = self._run(AGREEING_E4, DISAGREEING_PG)
        self.assertEqual(result.returncode, 1, msg=result.stderr)
        self.assertIn("DISAGREEMENTS", result.stderr)
        self.assertIn("kind_eq", result.stderr)

    def test_markdown_flag_produces_pipe_tables(self):
        result = self._run(AGREEING_E4, AGREEING_PG, extra_args=["--md"])
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        self.assertIn("| stage |", result.stdout)
        self.assertIn("| case |", result.stdout)

    def test_null_median_case_reports_note(self):
        result = self._run(AGREEING_E4, AGREEING_PG)
        self.assertIn("n/a: deviation: E4 has no score expression", result.stdout)


if __name__ == "__main__":
    unittest.main()
