#!/usr/bin/env python3
"""Run alternating isolated multimodel benchmark arms and preserve raw evidence."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import threading
import time
from typing import Any

POLL_SECONDS = 0.05


def tree_size(root: Path) -> tuple[int, int]:
    logical = allocated = 0
    if not root.exists():
        return 0, 0
    for path in root.rglob("*"):
        try:
            info = path.stat()
        except FileNotFoundError:
            continue
        if path.is_file():
            logical += info.st_size
            allocated += getattr(info, "st_blocks", (info.st_size + 511) // 512) * 512
    return logical, allocated


def rss(pid: int) -> int:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    except (FileNotFoundError, ProcessLookupError):
        pass
    return 0


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def run_arm(binary: Path, arm: dict[str, Any], directory: Path) -> dict[str, Any]:
    directory.mkdir()
    database = directory / "db"
    stdout_path = directory / "stdout.log"
    stderr_path = directory / "stderr.log"
    command = [
        str(binary), arm["engine"], str(arm["rows"]), str(arm["dimension"]),
        str(database), arm["policy"], arm["reader_mode"],
    ]
    peaks = {"logical_bytes": 0, "allocated_bytes": 0, "rss_bytes": 0}
    stop = threading.Event()
    samples = 0
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        def sample() -> None:
            nonlocal samples
            while not stop.wait(POLL_SECONDS):
                logical, allocated = tree_size(database)
                peaks["logical_bytes"] = max(peaks["logical_bytes"], logical)
                peaks["allocated_bytes"] = max(peaks["allocated_bytes"], allocated)
                peaks["rss_bytes"] = max(peaks["rss_bytes"], rss(process.pid))
                samples += 1
        thread = threading.Thread(target=sample, daemon=True)
        thread.start()
        started = time.monotonic()
        returncode = process.wait()
        elapsed = time.monotonic() - started
        stop.set(); thread.join()
    logical, allocated = tree_size(database)
    peaks["logical_bytes"] = max(peaks["logical_bytes"], logical)
    peaks["allocated_bytes"] = max(peaks["allocated_bytes"], allocated)
    lines = [line for line in stdout_path.read_text().splitlines() if line.strip()]
    record = {**arm, "command": command, "returncode": returncode,
              "wall_seconds": elapsed, "driver_sample_interval_seconds": POLL_SECONDS,
              "driver_samples": samples, "driver_sampled_peak": peaks,
              "final_tree_bytes": {"logical": logical, "allocated": allocated},
              "stdout": str(stdout_path), "stderr": str(stderr_path)}
    if returncode != 0 or not lines:
        record["failure"] = stderr_path.read_text()[-8000:]
        raise RuntimeError(json.dumps(record, sort_keys=True))
    record["raw"] = json.loads(lines[-1])
    return record


def flatten_numbers(value: Any, prefix: str = "") -> dict[str, float]:
    out: dict[str, float] = {}
    if isinstance(value, dict):
        for key, child in value.items():
            out.update(flatten_numbers(child, f"{prefix}.{key}" if prefix else key))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            out.update(flatten_numbers(child, f"{prefix}[{index}]"))
    elif isinstance(value, (int, float)) and not isinstance(value, bool):
        out[prefix] = float(value)
    return out


def summaries(arms: list[dict[str, Any]]) -> list[dict[str, Any]]:
    groups: dict[tuple[Any, ...], list[dict[str, Any]]] = {}
    for arm in arms:
        key = (arm["engine"], arm["policy"], arm["reader_mode"], arm["rows"], arm["dimension"])
        groups.setdefault(key, []).append(arm)
    result = []
    for key, members in sorted(groups.items()):
        completed = [member for member in members if not member["raw"]["result"].get("refused", False)]
        refused = [member for member in members if member["raw"]["result"].get("refused", False)]
        paths: dict[str, list[float]] = {}
        for member in completed:
            measured={"raw":member["raw"],"driver_sampled_peak":member["driver_sampled_peak"],"driver_wall_seconds":member["wall_seconds"],"final_tree_bytes":member["final_tree_bytes"]}
            for path, number in flatten_numbers(measured).items():
                paths.setdefault(path, []).append(number)
        result.append({
            "engine": key[0], "policy": key[1], "reader_mode": key[2],
            "rows": key[3], "dimension": key[4], "scheduled_trials": len(members),
            "completed_workloads": len(completed), "resource_refusals": len(refused),
            "refusal_reasons": sorted({member["raw"]["result"]["reason"] for member in refused}),
            "metrics": {path: {"median": statistics.median(values), "min": min(values), "max": max(values)}
                        for path, values in sorted(paths.items()) if len(values) == len(completed)},
        })
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--sizes", default="10000,100000,1000000")
    parser.add_argument("--reader-modes", default="none,batch,short,held")
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--dimension", type=int, default=32)
    parser.add_argument("--include-resumable", action=argparse.BooleanOptionalAction, default=True)
    args = parser.parse_args()
    if os.uname().sysname != "Linux":
        raise SystemExit("multimodel benchmark runtime is Linux-only")
    if args.root.exists():
        raise SystemExit("--root must be new")
    if args.trials < 3:
        raise SystemExit("at least three trials are required")
    binary = args.bin.resolve(strict=True); args.root.mkdir(parents=True)
    sizes = [int(value) for value in args.sizes.split(",")]
    modes = args.reader_modes.split(",")
    unknown_modes = set(modes) - {"none", "batch", "short", "held"}
    if unknown_modes:
        raise SystemExit(f"unknown reader modes: {sorted(unknown_modes)}")
    report: dict[str, Any] = {
        "format": "phase2-multimodel-driver-v2", "status": "candidate-evidence-not-public-acceptance",
        "binary": str(binary), "binary_sha256": sha256(binary), "poll_seconds": POLL_SECONDS,
        "peak_limit": "timed polling is discrete; transients between samples and unlinked SQLite temp files can be missed",
        "reader_scopes": {
            "none": "no concurrent reader",
            "batch": "one snapshot held across the first 256-row update commit of each CRUD round, then released",
            "short": "one snapshot held across each complete CRUD round",
            "held": "one snapshot held across all three CRUD rounds",
        },
        "arms": [], "capture_complete": False, "workload_complete": False, "complete": False,
    }
    report_path = args.root / "report.json"
    try:
        sequence = 0
        for rows in sizes:
            for mode in modes:
                for trial in range(args.trials):
                    order = ["e4", "sqlite"] if trial % 2 == 0 else ["sqlite", "e4"]
                    for engine in order:
                        arm = {"engine": engine, "policy": "atomic", "reader_mode": mode,
                               "rows": rows, "dimension": args.dimension, "trial": trial, "sequence": sequence}
                        record = run_arm(binary, arm, args.root / f"arm-{sequence:03d}-{engine}-atomic")
                        report["arms"].append(record); sequence += 1
                        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
                    if args.include_resumable:
                        arm = {"engine": "e4", "policy": "resumable", "reader_mode": mode,
                               "rows": rows, "dimension": args.dimension, "trial": trial, "sequence": sequence}
                        report["arms"].append(run_arm(binary, arm, args.root / f"arm-{sequence:03d}-e4-resumable")); sequence += 1
                        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        for rows in sizes:
            digests = {arm["raw"]["input_digest"] for arm in report["arms"] if arm["rows"] == rows}
            if len(digests) != 1:
                raise RuntimeError(f"input digest mismatch for N={rows}: {digests}")
        refusals = sum(bool(arm["raw"]["result"].get("refused", False)) for arm in report["arms"])
        report["completion_counts"] = {
            "scheduled": len(report["arms"]),
            "completed_workloads": len(report["arms"]) - refusals,
            "resource_refusals": refusals,
            "nonzero_process_exits": sum(arm["returncode"] != 0 for arm in report["arms"]),
        }
        report["summaries"] = summaries(report["arms"])
        report["capture_complete"] = True
        report["workload_complete"] = refusals == 0
        report["complete"] = report["workload_complete"]
    except BaseException as error:
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        report["finished_unix"] = time.time()
        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
