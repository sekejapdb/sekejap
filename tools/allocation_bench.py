#!/usr/bin/env python3
"""Native diagnostic: unchanged qualified E4, reserve-before-pwrite E4, SQLite.

The interposer is experimental, with all fstat/fallocate overhead timed. No
runtime code is changed. 100 ms per-file stat observations complement the
benchmark's existing 1 ms totals; both are lower bounds, never a quota proof.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

art = Path(sys.argv[1])
roots = {
    "<scratch>":
        "<scratch>",
    "<scratch>":
        "<scratch>",
}
qualified = Path(roots[str(art)])
binary = qualified / "bin/candidate/pagewal_bench"
probe = art / "allocation_probe"
source = art / "allocation_interpose.c"
if len(sys.argv) == 3:
    assert sys.argv[2] == "chunk"
    source = art / "allocation_chunk.c"
    art = art / "chunk"
    art.mkdir(exist_ok=False)
library = art / "allocation_interpose.so"
subprocess.run(["cc", "-O2", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
                str(source), "-o", str(library),
                "-ldl", "-pthread"], check=True)
hashes = {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
          for p in [binary, library, source, Path(__file__)]}
(art / "interpose-hashes.json").write_text(json.dumps(hashes, indent=2) + "\n")
env = dict(os.environ, MALLOC_ARENA_MAX="1")
env.pop("LD_PRELOAD", None)
reserve_env = dict(env, LD_PRELOAD=str(library))
with (art / "interpose-control.jsonl").open("x") as out, (art / "interpose-control.log").open("x") as err:
    subprocess.run(["prlimit", "--as=134217728", "--", str(probe),
                    str(art / "interpose-control.data"), "plain"],
                   env=reserve_env, stdout=out, stderr=err, check=True)
assert json.loads((art / "interpose-control.jsonl").read_text().splitlines()[-1])["verified"]
assert json.loads((art / "interpose-control.log").read_text().splitlines()[-1])["reservations"] > 0

def run(label, arm, n, cycles):
    out = art / "bench" / label
    out.mkdir(parents=True, exist_ok=False)
    command = ["prlimit", "--as=134217728", "--", str(binary), str(out),
               "sqlite" if arm == "sqlite" else "pagewal", str(n), "256",
               str(cycles), "mixed", "1000"]
    chosen = reserve_env if arm == "reserve" else env
    observations, peak = [], 0
    samples = errors = 0
    start = time.monotonic()
    with (out / "stdout.jsonl").open("w") as stdout, (out / "stderr.log").open("w") as stderr:
        process = subprocess.Popen(command, env=chosen, stdout=stdout, stderr=stderr)
        while True:
            files = []
            for p in sorted((out / "db").glob("**/*")):
                try:
                    s = p.stat()
                    if p.is_file():
                        files.append({"file": str(p.relative_to(out / "db")),
                                      "logical": s.st_size, "allocated": s.st_blocks * 512})
                except FileNotFoundError:
                    errors += 1
            total = sum(f["allocated"] for f in files)
            samples += 1
            if total > peak:
                peak = total
                observations.append({"elapsed": time.monotonic() - start, "files": files,
                                     "total_allocated": total})
            rc = process.poll()
            if rc is not None:
                break
            time.sleep(.1)
    record = {"arm": arm, "command": command, "returncode": rc,
              "sample_seconds": .1, "samples": samples, "disappeared_files": errors,
              "peak_events": observations}
    (out / "allocation-observations.json").write_text(json.dumps(record, indent=2) + "\n")
    assert rc == 0, (label, rc)
    r = json.loads((out / "report.json").read_text())
    assert r["reopen_verification"]["rows"] == n
    if arm == "reserve":
        stats = json.loads((out / "stderr.log").read_text().splitlines()[-1])
        assert stats["reservations"] > 0 and stats["reservation_failures"] == 0
    print(json.dumps({"label": label, "load_seconds": r["phases"][0]["seconds"],
                      "mutation_seconds": sum(p["seconds"] for p in r["phases"][1:]),
                      "peak_allocated": max(p["peak"]["allocated"] for p in r["phases"]),
                      "final_logical": r["phases"][-1]["final_logical"]}), flush=True)

run("smoke-reserve", "reserve", 10000, 2)
for trial, order in enumerate([("baseline", "reserve", "sqlite"),
                                ("sqlite", "baseline", "reserve"),
                                ("reserve", "sqlite", "baseline")], 1):
    for arm in order:
        run(f"mixed400k-t{trial}-{arm}", arm, 400000, 12)
(art / "bench-complete").write_text("All 9 measured arms and reserve smoke passed exact oracles.\n")
