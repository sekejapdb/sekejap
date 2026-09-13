#!/bin/bash
set -euo pipefail
base=<scratch>
binary="$base/collections-candidate"
run() {
  local label=$1 rows=$2 times=$3 cycles=$4 workload=$5 reader=$6 order=$7
  for engine in $order; do
    "$binary" "$base/matrix/$label" "$engine" "$rows" "$times" "$cycles" "$workload" "$reader" > "$base/matrix/$label-$engine.log" 2>&1
  done
}
mkdir "$base/matrix"
# Repeated primary gate, reversed engine order.
for rep in 1 2; do
  order='e4 sqlite'; if [[ $rep = 2 ]]; then order='sqlite e4'; fi
  for rows in 100000 400000; do
    run "mixed-$rows-r$rep" "$rows" off 12 mixed none "$order"
  done
done
for rows in 100000 400000; do
  run "load-$rows" "$rows" off 0 load none 'e4 sqlite'
  run "updates-$rows" "$rows" off 4 updates none 'sqlite e4'
  run "reinsert-$rows" "$rows" off 4 reinsert none 'e4 sqlite'
  run "held-$rows" "$rows" off 4 mixed held 'sqlite e4'
  run "rolling-$rows" "$rows" off 4 mixed rolling 'e4 sqlite'
  run "timestamps-$rows" "$rows" on 4 mixed none 'sqlite e4'
done
# Same new harness, pre-change kernel: isolate the packing implementation.
for rows in 100000 400000; do
  "$base/collections-baseline" "$base/ablation-$rows" e4 "$rows" off 12 mixed none > "$base/ablation-$rows.log" 2>&1
done
