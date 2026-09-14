#!/usr/bin/env python3
"""Read verified native metadata archives; no databases opened or modified."""
import hashlib
import json
from pathlib import Path
import statistics
import sys
import tarfile

result = {"version": 1, "runtime_changed": False, "physical_cap_proven": False,
          "variants": {}, "archives": []}
oracles = {}
for argument in sys.argv[1:]:
    host, variant, filename = argument.split(":", 2)
    archive = Path(filename)
    record = {"host": host, "variant": variant, "path": filename,
              "sha256": hashlib.sha256(archive.read_bytes()).hexdigest()}
    if variant == "complete":
        with tarfile.open(archive) as tar:
            members = {m.name: m for m in tar.getmembers()}
            tiny = {}
            for name in ("tiny-exact.jsonl", "tiny-chunk.jsonl"):
                rows = [json.loads(s) for s in tar.extractfile(members[name]).read().splitlines()]
                assert rows[-1] == {"verified": True, "bytes": 4096}
                assert rows[0]["logical"] == 4096
                assert rows[0]["allocated"] == (4096 if "exact" in name else 1048576)
                tiny[name] = rows
            plan = json.load(tar.extractfile(members["cleanup-prepared.json"]))
            assert len(plan["database_directories"]) == 20
            result.setdefault("complete_evidence", {})[host] = {"tiny_controls": tiny,
                                                                 "cleanup_plan": plan}
        result["archives"].append(record)
        continue
    data = {"controls": {}, "arms": [], "summary": {}}
    with tarfile.open(archive) as tar:
        members = {m.name.removeprefix("./"): m for m in tar.getmembers() if m.isfile()}
        def read(name):
            return tar.extractfile(members[name]).read().decode()
        def parsed(name):
            return json.loads(read(name))
        assert "bench-complete" in members
        for name in ("plain.jsonl", "reserve.jsonl", "trim.jsonl", "interpose-control.jsonl"):
            if name in members:
                rows = [json.loads(line) for line in read(name).splitlines()]
                assert rows[-1]["verified"] and rows[-1]["pages"] == 27648
                data["controls"][name] = rows
        data["source_hashes"] = parsed("interpose-hashes.json")
        for name in sorted(members):
            if not name.endswith("/report.json"):
                continue
            prefix = name.removesuffix("/report.json")
            r = parsed(name)
            n = r["rows"]
            assert n in (10000, 400000)
            assert r["cycles"] == (12 if n == 400000 else 2)
            assert len(r["phases"]) == r["cycles"] + 1
            assert r["reopen_verification"] == r["phases"][-1]["verification"]
            for i, phase in enumerate(r["phases"]):
                assert phase["cycle"] == i and phase["verification"]["rows"] == n
                assert phase["create_update_delete_counts"] == ([n, 0, 0] if i == 0 else [n//10, n//5, n//10])
                assert phase["peak"]["errors"] == 0
                key = (n, i)
                if key in oracles:
                    assert phase["verification"] == oracles[key]
                oracles[key] = phase["verification"]
            observation = parsed(prefix + "/allocation-observations.json")
            assert observation["returncode"] == 0
            stats = [json.loads(s) for s in read(prefix + "/stderr.log").splitlines() if s.startswith("{")]
            if observation["arm"] == "reserve":
                assert len(stats) == 1 and stats[0]["reservations"] > 0
                assert stats[0]["reservation_failures"] == 0
            data["arms"].append({"label": prefix.split("/")[-1], "arm": observation["arm"],
                                 "report": r, "observations": observation, "interposer": stats})
        assert len(data["arms"]) == 10
        for arm in ("baseline", "reserve", "sqlite"):
            runs = [a for a in data["arms"] if a["arm"] == arm and a["report"]["rows"] == 400000]
            assert len(runs) == 3
            reports = [a["report"] for a in runs]
            mutation = [sum(p["seconds"] for p in r["phases"][1:]) for r in reports]
            load = [r["phases"][0]["seconds"] for r in reports]
            logical = [max(p["peak"]["logical"] for p in r["phases"]) for r in reports]
            allocated = [max(p["peak"]["allocated"] for p in r["phases"]) for r in reports]
            loaded = {r["phases"][0]["final_logical"] for r in reports}
            final = {r["phases"][-1]["final_logical"] for r in reports}
            assert len(loaded) == len(final) == 1
            data["summary"][arm] = {
                "load_seconds": load, "load_median": statistics.median(load),
                "mutation_seconds": mutation, "mutation_median": statistics.median(mutation),
                "peak_logical": logical, "peak_allocated": allocated,
                "loaded_logical": next(iter(loaded)), "final_logical": next(iter(final)),
                "worst_allocated_to_loaded": max(allocated) / next(iter(loaded)),
            }
        data["mount"] = read("mount.txt") if "mount.txt" in members else None
    result["variants"].setdefault(variant, {})[host] = data
    result["archives"].append(record)
    print(host, variant, json.dumps(data["summary"], indent=2))
Path("docs/ALLOCATION_RESULTS.json").write_text(json.dumps(result, indent=2) + "\n")
