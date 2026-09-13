#!/bin/bash
set -euo pipefail
base=<scratch>
if cmp -s "$base/collections-baseline" "$base/collections-current"; then
  echo 'Refusing identical baseline/current binaries' >&2; exit 1
fi
if [[ ${1:-} != embedding-repeat ]]; then mkdir "$base/matrix"; fi
run() {
  local label=$1 n=$2 cycles=$3 workload=$4 reader=$5 times=$6 dim=$7 changes=$8 order=$9
  for arm in $order; do
    local binary="$base/collections-current" engine=e4
    if [[ $arm = baseline ]]; then binary="$base/collections-baseline"; fi
    if [[ $arm = sqlite ]]; then engine=sqlite; fi
    COLLECTION_VECTOR_DIM=$dim COLLECTION_CHANGE_VECTORS=$changes "$binary" \
      "$base/matrix/$label/$arm" "$engine" "$n" "$times" "$cycles" "$workload" "$reader" \
      > "$base/matrix/$label-$arm.log" 2>&1
  done
}
embedding_repeat() {
  run vectors-stable-r2 10000 4 updates none off 1536 0 'sqlite current baseline'
  run vectors-changing-r2 10000 4 updates none off 1536 1 'baseline current sqlite'
  run vectors-held-r2 10000 4 updates held off 1536 0 'sqlite current baseline'
}
if [[ ${1:-} = embedding-repeat ]]; then
  test -f "$base/matrix.complete"
  embedding_repeat
  date -u > "$base/embedding-repeat.complete"
  exit
fi
for rep in 1 2; do
  order='baseline current sqlite'; if [[ $rep = 2 ]]; then order='sqlite current baseline'; fi
  for n in 100000 400000; do
    run "mixed-$n-r$rep" "$n" 12 mixed none off 4 0 "$order"
  done
done
run load-100000 100000 0 load none off 4 0 'baseline current sqlite'
run updates-100000 100000 4 updates none off 4 0 'sqlite current baseline'
run reinsert-100000 100000 4 reinsert none off 4 0 'baseline current sqlite'
run held-100000 100000 4 mixed held off 4 0 'sqlite current baseline'
run rolling-100000 100000 4 mixed rolling off 4 0 'baseline current sqlite'
run timestamps-100000 100000 4 mixed none on 4 0 'sqlite current baseline'
run vectors-stable 10000 4 updates none off 1536 0 'baseline current sqlite'
run vectors-changing 10000 4 updates none off 1536 1 'sqlite current baseline'
run vectors-held 10000 4 updates held off 1536 0 'baseline current sqlite'
embedding_repeat
date -u > "$base/matrix.complete"
