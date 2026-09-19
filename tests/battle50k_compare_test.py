#!/usr/bin/env python3
"""Tests for tools/battle50k_compare.py.

stdlib only (unittest + subprocess), run with either:

    python3 -m pytest tests/battle50k_compare_test.py -q
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


def sweep_case(name, recall, median_us):
    return {
        "name": name,
        "kind": "approx",
        "queries": 50,
        "median_us": median_us,
        "p90_us": median_us * 1.2,
        "total_rows": 500,
        "first_keys": ["a"],
        "k": 10,
        "recall_at_k": recall,
        "note": f"sweep point {name}",
    }


# A fixture with a real recall-vs-latency SWEEP on `vec_ann_10`: E4's `ef`
# axis crosses recall 0.95 quickly and cheaply (ef=100, 150.0 us); Postgres's
# `sls` axis never reaches 0.95 even at its widest point (sls=800, recall
# 0.75) -- the shape the brief's PROBLEM section describes (equal ef / sls
# numerals do not mean equal recall), and the "never reached" branch of the
# headline the compare script must print instead of a ratio.
SWEEP_E4 = make_report(
    "e4",
    50000,
    "abc1234",
    stages=[{"name": "disk_bytes", "bytes": 10 * 1024 * 1024}],
    cases=[
        sweep_case("vec_ann_10@ef20", 0.70, 50.0),
        sweep_case("vec_ann_10@ef50", 0.85, 80.0),
        sweep_case("vec_ann_10@ef100", 0.97, 150.0),
        sweep_case("vec_ann_10@ef200", 0.99, 300.0),
        sweep_case("vec_ann_10@ef400", 1.00, 600.0),
    ],
)

SWEEP_PG = make_report(
    "postgres",
    50000,
    "abc1234",
    stages=[{"name": "disk_bytes", "bytes": 12 * 1024 * 1024}],
    cases=[
        sweep_case("vec_ann_10@sls50", 0.20, 30.0),
        sweep_case("vec_ann_10@sls100", 0.29, 45.0),
        sweep_case("vec_ann_10@sls200", 0.40, 70.0),
        sweep_case("vec_ann_10@sls400", 0.60, 120.0),
        sweep_case("vec_ann_10@sls800", 0.75, 220.0),
    ],
)


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

    def test_sweep_table_and_headline_when_pg_never_reaches_recall(self):
        result = self._run(SWEEP_E4, SWEEP_PG)
        self.assertEqual(result.returncode, 0, msg=result.stderr)

        # Every sweep point from both arms lands in the SWEEP table -- E4's
        # ef axis and Postgres's sls axis are distinct case names, so both
        # sets of rows appear rather than one crowding the other out.
        self.assertIn("SWEEP vec_ann_10", result.stdout)
        for label in ("ef20", "ef50", "ef100", "ef200", "ef400"):
            self.assertIn(label, result.stdout)
        for label in ("sls50", "sls100", "sls200", "sls400", "sls800"):
            self.assertIn(label, result.stdout)

        # The headline: E4 clears recall 0.95 at ef=100 for 150.0 us;
        # Postgres never clears it (best is sls=800 at recall 0.75), so the
        # line names that instead of printing a meaningless ratio.
        headline_lines = [
            line for line in result.stdout.splitlines() if line.startswith("HEADLINE vec_ann_10:")
        ]
        self.assertEqual(len(headline_lines), 1)
        headline = headline_lines[0]
        self.assertIn("no cross-arm ratio at recall>=0.95", headline)
        self.assertIn("e4 cheapest at recall>=0.95: ef100", headline)
        self.assertIn("median_us=150.0", headline)
        self.assertIn("pg never reached recall>=0.95; best is sls800", headline)
        self.assertIn("recall=0.750", headline)

    def test_sweep_headline_ratio_when_both_arms_reach_recall(self):
        pg_reaches = json.loads(json.dumps(SWEEP_PG))
        pg_reaches["cases"].append(sweep_case("vec_ann_10@sls1600", 0.96, 400.0))
        result = self._run(SWEEP_E4, pg_reaches)
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        headline_lines = [
            line for line in result.stdout.splitlines() if line.startswith("HEADLINE vec_ann_10:")
        ]
        self.assertEqual(len(headline_lines), 1)
        headline = headline_lines[0]
        # E4's cheapest >=0.95 point is ef100 at 150.0 us; Postgres's is the
        # new sls1600 point at 400.0 us -- so the ratio is 150/400.
        self.assertIn("e4 ef100 = 150.0 us", headline)
        self.assertIn("pg sls1600 = 400.0 us", headline)
        self.assertIn(f"e4/pg = {150.0 / 400.0:.3f}x", headline)


if __name__ == "__main__":
    unittest.main()
