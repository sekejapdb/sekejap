"""Summarize top-level Cargo targets without counting child-process summaries twice."""
import json
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
out = {}
for mode in ["workspace-default", "workspace-compact", "workspace-retained"]:
    path = root / "logs" / (mode + ".log")
    if not path.exists():
        out[mode] = {"status": "MISSING"}
        continue
    targets = []
    current = None
    for line in path.read_text().splitlines():
        if re.match(r"\s*(Running |Doc-tests )", line):
            if current is not None:
                targets.append(current)
            current = {"target": line.strip(), "result": None, "ignored": []}
        match = re.search(r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored", line)
        if match and current is not None:
            current["result"] = dict(status=match[1], passed=int(match[2]),
                                     failed=int(match[3]), ignored=int(match[4]))
        if current is not None and re.match(r"test .* \.\.\. ignored", line):
            current["ignored"].append(line)
    if current is not None:
        targets.append(current)
    missing = [t["target"] for t in targets if t["result"] is None]
    totals = {key: sum(t["result"][key] for t in targets if t["result"])
              for key in ["passed", "failed", "ignored"]}
    out[mode] = dict(totals=totals, targets=targets, unfinished_targets=missing)

print(json.dumps(out, indent=2))
