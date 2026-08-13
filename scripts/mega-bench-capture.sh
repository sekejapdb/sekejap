#!/usr/bin/env bash
#
# Emit a compact mega-benchmark result entry from the LAST run of
#   cargo bench --bench mega_benchmark
# (it reads the medians criterion persisted under target/criterion/).
#
# Usage:
#   cargo bench --bench mega_benchmark      # run the benchmark first
#   scripts/mega-bench-capture.sh           # print a compact entry to stdout
#   scripts/mega-bench-capture.sh --prepend # prepend the entry into the results log
#
# The mega benchmark is run ON DEMAND (not per release), so the results log is a
# hand-curated history: each entry is dated and tied to the commit it ran at, so
# the impact of a feature (e.g. snapshot reads) is visible across runs.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
LOG="eval/results/mega-benchmark.md"

DATE=$(date +%Y-%m-%d)
COMMIT=$(git rev-parse --short HEAD)
SUBJECT=$(git log -1 --format=%s | cut -c1-60)

# Optional wall-clock runtime (seconds) — set by mega-bench-run.sh, recorded in
# the entry so we can see how long the run took (and whether a change slows it).
ENTRY=$(python3 - "$DATE" "$COMMIT" "$SUBJECT" "${MEGA_BENCH_RUNTIME:-}" <<'PY'
import json, os, sys
date, commit, subject = sys.argv[1], sys.argv[2], sys.argv[3]
runtime = sys.argv[4] if len(sys.argv) > 4 else ""
base = "target/criterion"
def med(p):
    try: return json.load(open(p))["median"]["point_estimate"]
    except Exception: return None
def fmt(ns):
    if ns is None: return "—"
    if ns < 1e3: return f"{ns:.0f}ns"
    if ns < 1e6: return f"{ns/1e3:.1f}µs"
    return f"{ns/1e6:.2f}ms"
rows, wins, losses = [], 0, 0
for case in sorted(os.listdir(base)):
    if not case[0].isdigit():
        continue
    s = {e: med(f"{base}/{case}/{e}/new/estimates.json")
         for e in ("sekejap_sql", "sekejap_atomic", "sqlite")}
    seks = [v for v in (s["sekejap_sql"], s["sekejap_atomic"]) if v]
    best = min(seks) if seks else None
    sp = (s["sqlite"] / best) if (best and s["sqlite"]) else None
    if s["sqlite"] is None:
        verdict = "sekejap-only"
    elif sp >= 1:
        verdict = f"{sp:.1f}x"; wins += 1
    else:
        verdict = f"{1/sp:.1f}x SLOWER"; losses += 1
    rows.append((case, fmt(best), fmt(s["sqlite"]), verdict))
rt = ""
if runtime.isdigit():
    s = int(runtime); rt = f" · runtime {s//60}m {s%60}s"
print(f"## {date} — `{commit}` ({subject}){rt}")
print()
print("mode: resident (`CoreDB::open`) · vs in-memory SQLite · 20k venues + graph/spatial/vector")
print()
print("| scenario | sekejap | sqlite | vs sqlite |")
print("|---|---|---|---|")
for c, b, q, v in rows:
    print(f"| {c} | {b} | {q} | {v} |")
print()
print(f"**head-to-head: {wins} wins / {losses} loss** (+ sekejap-only cases)")
PY
)

if [ "${1:-}" = "--prepend" ]; then
  HEADER=$(sed -n '1,/^<!-- entries -->/p' "$LOG")
  BODY=$(sed '1,/^<!-- entries -->/d' "$LOG")
  { printf '%s\n\n%s\n\n%s\n' "$HEADER" "$ENTRY" "$BODY"; } > "$LOG"
  echo "prepended entry to $LOG"
else
  echo "$ENTRY"
fi
