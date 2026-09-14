"""Summarize completed three-arm native qualification without mixing hosts."""
import json
import statistics
import sys
from pathlib import Path

root = Path(sys.argv[1])
output = {}
for host in ("pi", "server"):
    raw = json.loads((root / host / "qualification.json").read_text())
    assert not raw["failures"], (host, raw["failures"])
    assert len(raw["records"]) == 81, host
    result = {"cases": {}, "caps": {}}
    for case in sorted({r["case"] for r in raw["records"]}):
        selected = [r for r in raw["records"] if r["case"] == case]
        if case.startswith("cap-"):
            result["caps"][case] = {r["arm"]: r["report"] for r in selected}
            assert len(selected) == 3
            continue
        assert len(selected) == 9
        arms = {}
        for arm in ("baseline", "candidate", "sqlite"):
            records = [r for r in selected if r["arm"] == arm]
            assert sorted(r["repetition"] for r in records) == [1, 2, 3]
            measures = {}
            for record in records:
                report = record["report"]
                if record["kind"] == "foundation_scale":
                    values = {"load_seconds": report["load"]["seconds"],
                              "final_logical_bytes": report["final_disk"]["logical"],
                              "final_allocated_bytes": report["final_disk"]["allocated"]}
                    values.update({p["operation"] + "_seconds": p["seconds"] for p in report["phases"]})
                else:
                    phases = report["phases"]
                    peak = max(p["peak"]["logical"] for p in phases)
                    values = {"load_seconds": phases[0]["seconds"],
                              "mutation_seconds": sum(p["seconds"] for p in phases[1:]),
                              "final_logical_bytes": phases[-1]["final_logical"],
                              "final_allocated_bytes": phases[-1]["final_allocated"],
                              "sampled_peak_logical_bytes": peak,
                              "sampled_peak_allocated_bytes": max(p["peak"]["allocated"] for p in phases),
                              "sampled_expansion_ratio": peak / phases[0]["final_logical"],
                              "sampled_allocated_expansion_ratio": max(p["peak"]["allocated"] for p in phases) / phases[0]["final_allocated"]}
                for name, value in values.items():
                    measures.setdefault(name, []).append(value)
            arms[arm] = {name: {"median": statistics.median(values), "trials": values}
                         for name, values in measures.items()}
        for rep in (1, 2, 3):
            oracles = [r["report"].get("verification", r["report"].get("reopen_verification"))
                       for r in selected if r["repetition"] == rep]
            assert all(o == oracles[0] for o in oracles), (host, case, rep)
        ratios = {name: arms["candidate"][name]["median"] / arms["sqlite"][name]["median"]
                  for name in arms["candidate"] if not name.startswith("sampled_expansion")}
        gains = {name: 1 - arms["candidate"][name]["median"] / arms["baseline"][name]["median"]
                 for name in arms["candidate"] if name.endswith("seconds")}
        result["cases"][case] = {"arms": arms, "candidate_sqlite_ratios": ratios,
                                  "candidate_reduction_vs_baseline": gains}
    output[host] = result
print(json.dumps(output, indent=2))
