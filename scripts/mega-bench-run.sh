#!/usr/bin/env bash
#
# Run the mega benchmark, time its wall-clock, and prepend a compact entry (with
# the runtime) into eval/results/mega-benchmark.md. One command for a full capture:
#
#   scripts/mega-bench-run.sh            # run + capture (prepend, with runtime)
#   scripts/mega-bench-run.sh --no-write # run + print entry to stdout, don't write
#
# The mega benchmark is heavy (~10-15 min); this records how long it took so a
# change that slows the harness itself is visible.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

start=$(date +%s)
cargo bench --bench mega_benchmark
end=$(date +%s)
export MEGA_BENCH_RUNTIME=$((end - start))

echo "mega benchmark wall-clock: $((MEGA_BENCH_RUNTIME / 60))m $((MEGA_BENCH_RUNTIME % 60))s"
if [ "${1:-}" = "--no-write" ]; then
  scripts/mega-bench-capture.sh
else
  scripts/mega-bench-capture.sh --prepend
fi
