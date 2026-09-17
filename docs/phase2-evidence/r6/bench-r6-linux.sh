#!/usr/bin/env bash
set -euo pipefail
R=<scratch>
export PATH=/usr/local/cargo/bin:$PATH CARGO_HOME=<scratch> CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=$R/target TMPDIR=<scratch> SQLITE_TMPDIR=<scratch>
# Stage the new harness separately; don't change sources beneath the live suite.
while [ ! -f "$R/graph-r2.exit" ]; do sleep 5; done
[ "$(cat "$R/graph-r2.exit")" = 0 ]
cp "$R/phase2_scalar_bench-r6.rs" "$R/src/src/bin/phase2_scalar_bench.rs"
cd "$R/src"
mkdir -p "$R/bench-r6-binaries"
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo build --release --locked --offline --bin phase2_scalar_bench "${flags[@]}" > "$R/logs/bench-r6-build-$mode.log" 2>&1
 cp "$R/target/release/phase2_scalar_bench" "$R/bench-r6-binaries/$mode"
done
sha256sum "$R/bench-r6-binaries/"* > "$R/logs/bench-r6-binaries.sha256"
for mode in default retained; do
 bin="$R/bench-r6-binaries/$mode"
 for rep in 1 2 3; do
  engines=(e4 sqlite)
  if [ "$rep" = 2 ]; then engines=(sqlite e4); fi
  for engine in "${engines[@]}"; do
   "$bin" "$engine" 100000 "$R/scalar-r6-$mode-100000-$engine-$rep" atomic > "$R/logs/bench-r6-$mode-100000-$engine-$rep-atomic.json"
   printf 'BENCH_R6_PASS %s %s %s atomic\n' "$mode" "$engine" "$rep"
  done
 done
 "$bin" e4 100000 "$R/scalar-r6-$mode-100000-e4-resumable" resumable > "$R/logs/bench-r6-$mode-100000-e4-resumable.json"
 printf 'BENCH_R6_PASS %s e4 resumable\n' "$mode"
done
