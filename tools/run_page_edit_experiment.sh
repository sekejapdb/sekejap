#!/bin/bash
set -euo pipefail
base=<scratch>
export TMPDIR="$base/tmp" SQLITE_TMPDIR="$base/tmp" COLLECTION_BATCH=1000
mkdir "$base/matrix"
for rep in 1 2; do
  order='baseline scratch cell combined sqlite'
  if [[ $rep = 2 ]]; then order='sqlite combined cell scratch baseline'; fi
  for case in mixed updates; do
    label="$case-100000-r$rep"
    for arm in $order; do
      engine=e4; binary="$base/$arm"
      if [[ $arm = sqlite ]]; then engine=sqlite; binary="$base/baseline"; fi
      "$binary" "$base/matrix/$label/$arm" "$engine" 100000 off 4 "$case" none > "$base/matrix/$label-$arm.log" 2>&1
      echo "completed $label $arm"
    done
  done
done
date -u > "$base/matrix.complete"
