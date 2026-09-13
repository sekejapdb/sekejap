#!/bin/bash
set -euo pipefail
root=<scratch>
art="$root/artifacts/reuse-20260912"
export TMPDIR="$art/tmp" SQLITE_TMPDIR="$art/tmp"
test -f "$art/build-tests.complete"
if cmp -s "$art/baseline" "$art/candidate"; then exit 1; fi
mkdir "$art/matrix"
for spec in 'batch-1-r1 1 1000 2 none' 'batch-100-r1 100 10000 3 none' 'batch-1-r2 1 1000 2 none' 'batch-100-r2 100 10000 3 none' 'mixed-400000 1000 400000 12 none' 'held-100000 1000 100000 4 held'; do
  read -r label batch rows cycles reader <<< "$spec"
  order='baseline candidate sqlite'
  if [[ $label = *-r2 ]]; then order='sqlite candidate baseline'; fi
  for arm in $order; do
    engine=e4; binary="$art/$arm"
    if [[ $arm = sqlite ]]; then engine=sqlite; binary="$art/baseline"; fi
    COLLECTION_BATCH=$batch python3 "$root/packing_time.py" prlimit --as=134217728 -- "$binary" "$art/matrix/$label/$arm" "$engine" "$rows" off "$cycles" mixed "$reader" > "$art/matrix/$label-$arm.log" 2>&1
    echo "completed $label $arm"
  done
done
date -Is > "$art/matrix.complete"
