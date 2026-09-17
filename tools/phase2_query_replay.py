#!/usr/bin/env python3
"""Compare baseline/candidate/SQLite reads of pinned completed R7 databases.

No fixture generation, writes, rebuilds or deletion. Every process is followed
by source hash checks. Timings describe post-CRUD queries, not original R7's
pre-CRUD timings, and do not replace the full scale/disk/CRUD matrix.
"""
import argparse
import json
import math
from pathlib import Path
import subprocess
import sys

from format_reference_compat import digest, inventory, new_artifact_path

BENCHMARK_HASH = "5034ad305932ecf60b28a1c9a61be5a08935cd56f2beeb09807ca24addb8e71e"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def same_hits(left, right):
    return left.keys() == right.keys() and all(
        len(left[seed]) == len(right[seed]) and all(
            a[0] == b[0] and math.isfinite(a[1]) and math.isfinite(b[1])
            and abs(a[1] - b[1]) <= 1e-12
            for a, b in zip(left[seed], right[seed])
        ) for seed in left
    )


def select_source(report_path, expected_hash):
    report_path = report_path.resolve(strict=True)
    require(digest(report_path) == expected_hash, "benchmark report hash differs")
    data = json.loads(report_path.read_text())
    require(data.get("format") == "phase2-multimodel-driver-v2", "unsupported benchmark report")
    require(data.get("capture_complete") is True, "benchmark matrix is still incomplete")
    require(data.get("binary_sha256") == BENCHMARK_HASH, "not the preserved R7 binary")
    require(len(data["arms"]) == 36, "R7 matrix must contain all 36 arms")
    selected = {}
    for engine in ("e4", "sqlite"):
        eligible = [a for a in data["arms"] if a["engine"] == engine and a["reader_mode"] == "none"
                    and a["returncode"] == 0 and not a["raw"]["result"].get("refused")
                    and len(a["raw"]["result"].get("crud", [])) == 3]
        # Atomic first; if a large atomic index build refused, an otherwise
        # complete resumable corpus has the same final logical query workload.
        eligible.sort(key=lambda a: (a["policy"] != "atomic", a["trial"]))
        require(eligible, f"no completed no-reader {engine} corpus in {report_path}")
        arm = eligible[0]
        root = Path(arm["command"][4]).resolve(strict=True)
        require(root.is_relative_to(report_path.parent), "source is outside pinned report directory")
        require(root.is_dir() and not Path(arm["command"][4]).is_symlink(), "source must be an ordinary directory")
        require(arm["raw"]["result"]["engine"] == engine, "engine identity differs")
        require(all(c["cycle"] == i and c["updated"] == arm["rows"] for i, c in enumerate(arm["raw"]["result"]["crud"])), "not three complete full-population CRUD rounds")
        selected[engine] = {"path": root, "arm": arm}
    require(selected["e4"]["arm"]["rows"] == selected["sqlite"]["arm"]["rows"], "different populations")
    require(selected["e4"]["arm"]["dimension"] == selected["sqlite"]["arm"]["dimension"], "different vector dimensions")
    require(selected["e4"]["arm"]["raw"]["input_digest"] == selected["sqlite"]["arm"]["raw"]["input_digest"], "different generated inputs")
    return report_path, selected


def main():
    require(sys.flags.optimize == 0, "Python optimization is forbidden for qualification")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-bin", type=Path, required=True)
    parser.add_argument("--candidate-bin", type=Path, required=True)
    parser.add_argument("--report", nargs=2, action="append", required=True, metavar=("PATH", "SHA256"))
    parser.add_argument("--work", type=Path, required=True)
    args = parser.parse_args()
    require(sys.platform == "linux", "database replay runs only on the authorized Linux host")
    binaries = {label: path.resolve(strict=True) for label, path in
                (("baseline", args.baseline_bin), ("candidate", args.candidate_bin))}
    hashes = {label: digest(path) for label, path in binaries.items()}
    require(hashes["baseline"] != hashes["candidate"], "identical A/B binaries")
    sources = [select_source(Path(path), checksum) for path, checksum in args.report]
    work = new_artifact_path(args.work)
    for report, engines in sources:
        require(not work.is_relative_to(report.parent), "output is inside a protected benchmark tree")
        require(not report.is_relative_to(work), "output contains protected evidence")
        for source in engines.values():
            require(not work.is_relative_to(source["path"]) and not source["path"].is_relative_to(work), "output overlaps database")
    for binary in binaries.values():
        require(not binary.is_relative_to(work), "output contains input executable")
    work.mkdir(parents=True)
    output = {"format": "phase2-query-replay-report-v1", "result": "RUNNING",
              "claim_scope": "read-only post-three-CRUD query A/B and SQLite; no cold-cache, whole-CRUD or disk-cost claim",
              "binaries": {label: {"path": str(path), "sha256": hashes[label]} for label, path in binaries.items()},
              "sources": [], "arms": [], "commands": []}
    preserved = {}

    def run(binary, *argv):
        command = [str(binary), *map(str, argv)]
        result = subprocess.run(command, text=True, capture_output=True)
        output["commands"].append({"argv": command, "exit_code": result.returncode, "stdout": result.stdout, "stderr": result.stderr})
        require(result.returncode == 0, f"replay failed: {command}\n{result.stderr}")
        return json.loads(result.stdout)

    try:
        for label, binary in binaries.items():
            version = run(binary, "--version")
            require(version["harness"] == "phase2-query-replay-v1", "wrong replay executable")
            require(version["engine_revision"] != "unrecorded", "binary provenance missing")
            output["binaries"][label]["version"] = version
        baseline_features = output["binaries"]["baseline"]["version"]["compile_features"]
        candidate_features = output["binaries"]["candidate"]["version"]["compile_features"]
        require(baseline_features == candidate_features and len(baseline_features) == 4
                and all(value is True for value in baseline_features.values()),
                "A/B requires the same retained build configuration")
        for report, engines in sources:
            for source in engines.values():
                preserved[source["path"]] = inventory(source["path"])
            rows, dim = engines["e4"]["arm"]["rows"], engines["e4"]["arm"]["dimension"]
            output["sources"].append({"report": str(report), "report_sha256": digest(report), "rows": rows, "dimension": dim,
                                     "databases": {engine: {"path": str(s["path"]), "policy": s["arm"]["policy"], "trial": s["arm"]["trial"], "files": preserved[s["path"]]} for engine, s in engines.items()}})
            expected_hits = None
            order = ("baseline", "candidate", "sqlite")
            for trial in range(3):
                for label in order[trial:] + order[:trial]:
                    engine = "sqlite" if label == "sqlite" else "e4"
                    binary = binaries["baseline" if label == "sqlite" else label]
                    root = engines[engine]["path"]
                    try:
                        result = run(binary, engine, root, rows, dim, 5)
                    finally:
                        require(inventory(root) == preserved[root], f"read-only replay changed {root}")
                    require(result["format"] == "phase2-query-replay-v1", "wrong replay report")
                    raw = result["result"]
                    require(raw["engine"] == engine and raw["people"] == rows, "wrong replay population")
                    samples = raw["samples"]
                    require(len(samples) == 15, "incomplete repeat/seed matrix")
                    require({(s["repetition"], s["seed"]) for s in samples} == {(r, seed) for r in range(5) for seed in (17, 73, 211)}, "duplicate/missing replay samples")
                    hits = {seed: next(s["hits"] for s in samples if s["seed"] == seed) for seed in (17, 73, 211)}
                    for sample in samples:
                        require(math.isfinite(sample["seconds"]) and sample["seconds"] >= 0, "invalid timing")
                        require(sample["hits"] == hits[sample["seed"]], "unstable answers across repetitions")
                    if expected_hits is None:
                        expected_hits = hits
                    require(same_hits(hits, expected_hits), "baseline/candidate/SQLite answers differ")
                    output["arms"].append({"label": label, "trial": trial, "rows": rows, "dimension": dim, "raw": result})
                    (work / "REPORT.json").write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
                    print(json.dumps({"label": label, "trial": trial, "rows": rows, "dimension": dim, "result": "PASS"}), flush=True)
        require(len(output["arms"]) == 9 * len(sources), "incomplete process matrix")
        for path, checksum in args.report:
            require(digest(Path(path)) == checksum, "source report changed during replay")
        for label, binary in binaries.items():
            require(digest(binary) == hashes[label], "input executable changed")
        output["result"] = "PASS"
    finally:
        unchanged = all(inventory(path) == before for path, before in preserved.items())
        output["source_unchanged"] = unchanged
        if output["result"] == "RUNNING" or not unchanged:
            output["result"] = "FAIL"
        (work / "REPORT.json").write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
        require(unchanged, "read-only replay changed a source database")


if __name__ == "__main__":
    main()
